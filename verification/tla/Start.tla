------------------------------- MODULE Start -------------------------------
EXTENDS Naturals, FiniteSets
(***************************************************************************)
(* Unified start API, revision 3 (0.9; design docs/design/unified-start.md).*)
(*                                                                         *)
(* One parent actor P (managed in group G or unmanaged; it may trap exits) *)
(* starts children with spawn, start, start_link or start_monitor, each    *)
(* with a default or explicit placement (G, G2 = another group of P's      *)
(* system, U = unmanaged, same = placement_of) and possibly the one shared *)
(* name.                                                                   *)
(*                                                                         *)
(* Stage 1 is one Commit under the design's lock order: P's scope gate (for*)
(* owned children), the target group, P's exit state, P's edge table, the  *)
(* registry. A refused start is never observable.                         *)
(*                                                                         *)
(* Every start has an immutable startup commitment (ready, failed,         *)
(* ignored, timedout; commit0 is its first value and must never change).   *)
(* Waiting starts also have a waiter (waiting, consumed, abandoned).       *)
(*                                                                         *)
(* Death is D (Die: edges close, notices taken), destruction, then         *)
(* Release (name and slot) and Publish (notices disposed of through the    *)
(* commitment and the waiter; answers become final). The exit reason can   *)
(* still change before D (a kill), and errors report the final reason. A   *)
(* waiter may answer before publication only after its cleanup deadline   *)
(* (dl), with pend = TRUE.                                                 *)
(*                                                                         *)
(* P's own death is D (PDie: edges close, its link notices taken) then     *)
(* publication (PPublish: notices and the scope's polite Shutdown reach    *)
(* the children; P's slot is released). P's generation is done only after *)
(* P's publication and every owned child's.                                *)
(***************************************************************************)
CONSTANTS
    Kids,             \* child names
    Kinds,            \* subset of {"spawn","start","link","monitor"}
    Explicit,         \* subset of {"none","G","G2","U","same"}
    ParentPlaces,     \* subset of {"G","U"}
    Cap,              \* live actors G admits (P included); held until release
    Named,            \* children may request the one shared name
    TimeoutEnabled,   \* the init timer may fire
    KillEnabled,      \* an outside kill (ChildHandle::kill) may hit a child
    AdminStopEnabled, \* an administrative stop may hit a child in started()
    ParentCanExit,    \* P may exit (Normal or Panic)
    CloseEnabled,     \* G may close (system shutdown)
    OwnerCloseEnabled,\* P's scope may close (its supervisor's shutdown began)
    MaxMsgs,          \* messages others send to each child
    ChildCanTrap,     \* started() may set trap_exit
    UnlinkEnabled,    \* a ready linked child may unlink from P
    ScopeEnabled,     \* model P's generation end and its scope deadline (S8)
    Fault             \* "None" or a negative control (README, Unified start)

None == "none"
NonNormal == {"Panic", "Killed", "Shutdown"}
Reasons == {"Normal"} \cup NonNormal
Phases == {"idle", "begun", "reg1", "starting", "ready", "exiting", "dead",
           "namerel", "released", "published", "aborted"}
Commits == {None, "ready", "failed", "ignored", "timedout"}
FailCommits == {"failed", "ignored", "timedout"}
Waits == {"none", "waiting", "abandoned", "consumed"}
Results == {"none", "pending", "ok", "failed", "ignored", "timeout", "closed",
            "capacity", "already", "parentexited", "system"}
Outcomes == {"ready", "continue", "panic", "stopN", "stopS", "error", "ignore"}
Alive(p) == p \in {"run", "exiting"}          \* P's mailbox still admits

VARIABLES
    pst, pplace, ptrap, pwhy, pcause, gopen, ownerOpen, gdone, sfired, pn,
    kind, expl, wname,
    phase, place, admOpen, owned, ownOpenAt, readyOpen, edgeWhenRun,
    edge, mon, linkEver, monEver,
    began, killed, skill, xkill, astop, pex, qpex, pexAbn, ctrap,
    commit, commit0, initR, why, wait, res, resR, pend, dl,
    lnote, mnote, pubAlive, div, divEver, handed, lost,
    cont, msgs, early,
    reg, seen, blamed, slots,
    exitN, downN, exitR, downR, obN, obR, dlvOk

pV == <<pst, pplace, ptrap, pwhy, pcause, gopen, ownerOpen, gdone, sfired, pn>>
oV == <<kind, expl, wname>>
sV == <<phase, place, admOpen, owned, ownOpenAt, readyOpen, edgeWhenRun>>
eV == <<edge, mon, linkEver, monEver>>
xV == <<began, killed, skill, xkill, astop, pex, qpex, pexAbn, ctrap>>
cV == <<commit, commit0, initR, why, wait, res, resR, pend, dl>>
nV == <<lnote, mnote, pubAlive, div, divEver, handed, lost>>
mV == <<cont, msgs, early>>
rV == <<reg, seen, blamed, slots>>
dV == <<exitN, downN, exitR, downR, obN, obR, dlvOk>>
vars == <<pV, oV, sV, eV, xV, cV, nV, mV, rV, dV>>

Waiting(k) == k \in {"start", "link", "monitor"}
LiveP == {"starting", "ready", "exiting", "dead"}     \* holds name and slot
PSlot == IF pplace = "G" /\ pst # "published" /\ ~(Fault = "ParentSlotAtD" /\ pst = "dead")
         THEN 1 ELSE 0
Used == Cardinality(slots) + PSlot

Expected(c) ==
    CASE expl[c] = "same" -> pplace
      [] expl[c] \in {"G", "G2", "U"} -> expl[c]
      [] kind[c] \in {"link", "monitor"} -> pplace
      [] OTHER -> "U"
Target(c) ==
    IF expl[c] = None /\ kind[c] \in {"link", "monitor"} /\ Fault = "NoInherit"
    THEN "U" ELSE Expected(c)
IsOwned(c, t) == kind[c] = "link" /\ pplace = "G" /\ t \in {"G", "G2"}

TypeOK ==
    /\ pst \in {"run", "exiting", "dead", "published"} /\ pplace \in {"G", "U"}
    /\ ptrap \in BOOLEAN /\ pwhy \in Reasons \cup {None}
    /\ pcause \in Kids \cup {None} /\ gopen \in BOOLEAN /\ ownerOpen \in BOOLEAN
    /\ gdone \in BOOLEAN /\ sfired \in BOOLEAN /\ pn \in [Kids -> Reasons \cup {None}]
    /\ kind \in [Kids -> {"spawn", "start", "link", "monitor", None}]
    /\ expl \in [Kids -> {"G", "G2", "U", "same", None}]
    /\ wname \in [Kids -> BOOLEAN]
    /\ phase \in [Kids -> Phases] /\ place \in [Kids -> {"G", "G2", "U", None}]
    /\ admOpen \in [Kids -> BOOLEAN] /\ owned \in [Kids -> BOOLEAN]
    /\ ownOpenAt \in [Kids -> BOOLEAN] /\ readyOpen \in [Kids -> BOOLEAN]
    /\ edgeWhenRun \in [Kids -> BOOLEAN]
    /\ edge \in [Kids -> BOOLEAN] /\ mon \in [Kids -> BOOLEAN]
    /\ linkEver \in [Kids -> BOOLEAN] /\ monEver \in [Kids -> BOOLEAN]
    /\ began \in [Kids -> BOOLEAN] /\ killed \in [Kids -> BOOLEAN]
    /\ skill \in [Kids -> BOOLEAN] /\ xkill \in [Kids -> BOOLEAN]
    /\ astop \in [Kids -> BOOLEAN]
    /\ pex \in [Kids -> Reasons \cup {None}] /\ qpex \in [Kids -> Reasons \cup {None}]
    /\ pexAbn \in [Kids -> BOOLEAN] /\ ctrap \in [Kids -> BOOLEAN]
    /\ commit \in [Kids -> Commits] /\ commit0 \in [Kids -> Commits]
    /\ initR \in [Kids -> Reasons \cup {None}] /\ why \in [Kids -> Reasons \cup {None}]
    /\ wait \in [Kids -> Waits] /\ res \in [Kids -> Results]
    /\ resR \in [Kids -> Reasons \cup {None}]
    /\ pend \in [Kids -> BOOLEAN] /\ dl \in [Kids -> BOOLEAN]
    /\ lnote \in [Kids -> BOOLEAN] /\ mnote \in [Kids -> BOOLEAN]
    /\ pubAlive \in [Kids -> BOOLEAN]
    /\ div \in [Kids -> Reasons \cup {None}] /\ divEver \in [Kids -> BOOLEAN]
    /\ handed \in [Kids -> BOOLEAN] /\ lost \in [Kids -> BOOLEAN]
    /\ cont \in [Kids -> BOOLEAN] /\ msgs \in [Kids -> 0..MaxMsgs]
    /\ early \in [Kids -> BOOLEAN]
    /\ reg \subseteq Kids /\ seen \in [Kids -> BOOLEAN]
    /\ blamed \subseteq Kids /\ slots \subseteq Kids
    /\ exitN \in [Kids -> 0..3] /\ downN \in [Kids -> 0..3]
    /\ exitR \in [Kids -> Reasons \cup {None}] /\ downR \in [Kids -> Reasons \cup {None}]
    /\ obN \in [Kids -> 0..3] /\ obR \in [Kids -> Reasons \cup {None}]
    /\ dlvOk \in [Kids -> BOOLEAN]

F(v) == [c \in Kids |-> v]
Init ==
    /\ pst = "run" /\ pplace \in ParentPlaces /\ ptrap \in BOOLEAN
    /\ pwhy = None /\ pcause = None /\ gopen = TRUE /\ ownerOpen = TRUE
    /\ gdone = FALSE /\ sfired = FALSE /\ pn = F(None)
    /\ kind = F(None) /\ expl = F(None) /\ wname = F(FALSE)
    /\ phase = F("idle") /\ place = F(None) /\ admOpen = F(FALSE)
    /\ owned = F(FALSE) /\ ownOpenAt = F(FALSE) /\ readyOpen = F(FALSE)
    /\ edgeWhenRun = F(FALSE)
    /\ edge = F(FALSE) /\ mon = F(FALSE) /\ linkEver = F(FALSE) /\ monEver = F(FALSE)
    /\ began = F(FALSE) /\ killed = F(FALSE) /\ skill = F(FALSE) /\ xkill = F(FALSE)
    /\ astop = F(FALSE) /\ pex = F(None) /\ qpex = F(None) /\ pexAbn = F(FALSE)
    /\ ctrap = F(FALSE)
    /\ commit = F(None) /\ commit0 = F(None) /\ initR = F(None) /\ why = F(None)
    /\ wait = F("none") /\ res = F("none") /\ resR = F(None) /\ pend = F(FALSE)
    /\ dl = F(FALSE)
    /\ lnote = F(FALSE) /\ mnote = F(FALSE) /\ pubAlive = F(FALSE)
    /\ div = F(None) /\ divEver = F(FALSE) /\ handed = F(FALSE) /\ lost = F(FALSE)
    /\ cont = F(FALSE) /\ msgs = F(0) /\ early = F(FALSE)
    /\ reg = {} /\ seen = F(FALSE) /\ blamed = {} /\ slots = {}
    /\ exitN = F(0) /\ downN = F(0) /\ exitR = F(None) /\ downR = F(None)
    /\ obN = F(0) /\ obR = F(None) /\ dlvOk = F(FALSE)

-----------------------------------------------------------------------------
(* Notices reaching P (only while P's mailbox admits: before P's D). *)

DeliverLink(c, r) ==
    IF ~Alive(pst) THEN UNCHANGED <<exitN, exitR, pst, pwhy, pcause>>
    ELSE IF ptrap
    THEN /\ exitN' = [exitN EXCEPT ![c] = @ + 1]
         /\ exitR' = [exitR EXCEPT ![c] = r]
         /\ UNCHANGED <<pst, pwhy, pcause>>
    ELSE IF r # "Normal" /\ pst = "run"
    THEN /\ pst' = "exiting" /\ pwhy' = r /\ pcause' = c
         /\ UNCHANGED <<exitN, exitR>>
    ELSE UNCHANGED <<exitN, exitR, pst, pwhy, pcause>>

DeliverDown(c, r) ==
    IF ~Alive(pst) THEN UNCHANGED <<downN, downR>>
    ELSE /\ downN' = [downN EXCEPT ![c] = @ + 1]
         /\ downR' = [downR EXCEPT ![c] = r]

\* The startup commitment: set once; commit0 keeps the first value.
SetCommit(c, v) ==
    /\ commit' = [commit EXCEPT ![c] = v]
    /\ commit0' = IF commit0[c] = None THEN [commit0 EXCEPT ![c] = v] ELSE commit0

-----------------------------------------------------------------------------
(* Stage 1, in the caller. *)

Begin(c) ==
    /\ phase[c] = "idle"
    /\ \E k \in Kinds, e \in Explicit, n \in (IF Named THEN BOOLEAN ELSE {FALSE}):
         /\ kind' = [kind EXCEPT ![c] = k]
         /\ expl' = [expl EXCEPT ![c] = IF e = "none" THEN None ELSE e]
         /\ wname' = [wname EXCEPT ![c] = n]
    /\ phase' = [phase EXCEPT ![c] = "begun"]
    /\ UNCHANGED <<pV, place, admOpen, owned, ownOpenAt, readyOpen, edgeWhenRun,
                   eV, xV, cV, nV, mV, rV, dV>>

PrepareFail(c) ==
    /\ phase[c] = "begun"
    /\ phase' = [phase EXCEPT ![c] = "aborted"]
    /\ res' = [res EXCEPT ![c] = "system"]
    /\ UNCHANGED <<pV, oV, place, admOpen, owned, ownOpenAt, readyOpen, edgeWhenRun,
                   eV, xV, commit, commit0, initR, why, wait, resR, pend, dl,
                   nV, mV, rV, dV>>

RefusalNoName(c) ==
    LET t == Target(c) IN
    CASE IsOwned(c, t) /\ ~ownerOpen /\ Fault # "OwnerUnchecked" -> "closed"
      [] t = "G" /\ ~gopen /\ Fault # "AdmitClosed" -> "closed"
      [] t = "G" /\ Used >= Cap /\ Fault # "IgnoreCap" -> "capacity"
      [] kind[c] \in {"link", "monitor"} /\ pst # "run" /\ ~(Fault = "LinkCheckLate" /\ pst # "published")
             -> "parentexited"
      [] OTHER -> None
Refusal(c) ==
    IF RefusalNoName(c) # None THEN RefusalNoName(c)
    ELSE IF wname[c] /\ reg # {} /\ Fault # "DoubleRegister" THEN "already"
    ELSE None

\* Who a refusal was caused by (a competing start's observation).
Blame(r) == CASE r = "capacity" -> slots [] r = "already" -> reg [] OTHER -> {}

Admit(c, t, lk, mn) ==
    /\ phase' = [phase EXCEPT ![c] = "starting"]
    /\ place' = [place EXCEPT ![c] = t]
    /\ admOpen' = [admOpen EXCEPT ![c] = gopen \/ t # "G"]
    /\ owned' = [owned EXCEPT ![c] = IsOwned(c, t)]
    /\ ownOpenAt' = [ownOpenAt EXCEPT ![c] = ownerOpen]
    /\ edgeWhenRun' = [edgeWhenRun EXCEPT ![c] = pst = "run"]
    /\ edge' = [edge EXCEPT ![c] = lk]
    /\ mon' = [mon EXCEPT ![c] = mn]
    /\ linkEver' = [linkEver EXCEPT ![c] = lk]
    /\ monEver' = [monEver EXCEPT ![c] = mn]
    /\ slots' = IF t = "G" THEN slots \cup {c} ELSE slots
    /\ wait' = [wait EXCEPT ![c] = IF Waiting(kind[c]) THEN "waiting" ELSE "none"]
    /\ res' = [res EXCEPT ![c] = IF Waiting(kind[c]) THEN "pending" ELSE "ok"]

Commit(c) ==
    /\ phase[c] = "begun"
    /\ Fault # "SplitCommit" \/ ~wname[c]
    /\ LET t == Target(c)
           r == Refusal(c)
       IN IF r # None
          THEN /\ phase' = [phase EXCEPT ![c] = "aborted"]
               /\ res' = [res EXCEPT ![c] = r]
               /\ blamed' = blamed \cup Blame(r)
               /\ UNCHANGED <<place, admOpen, owned, ownOpenAt, edgeWhenRun, eV,
                              wait, reg, slots>>
          ELSE /\ Admit(c, t, kind[c] = "link" /\ Fault # "LinkAfterStarted",
                        kind[c] = "monitor")
               /\ reg' = IF wname[c] THEN reg \cup {c} ELSE reg
               /\ UNCHANGED blamed
    /\ UNCHANGED <<pV, oV, readyOpen, xV, commit, commit0, initR, why, resR, pend, dl,
                   nV, mV, seen, dV>>

\* SplitCommit (negative control): the name is published before the rest.
RegisterFirst(c) ==
    /\ Fault = "SplitCommit"
    /\ phase[c] = "begun" /\ wname[c] /\ reg = {}
    /\ reg' = reg \cup {c}
    /\ phase' = [phase EXCEPT ![c] = "reg1"]
    /\ UNCHANGED <<pV, oV, place, admOpen, owned, ownOpenAt, readyOpen, edgeWhenRun,
                   eV, xV, cV, nV, mV, seen, blamed, slots, dV>>
CommitRest(c) ==
    /\ phase[c] = "reg1"
    /\ LET t == Target(c)
           r == RefusalNoName(c)
       IN IF r # None
          THEN /\ phase' = [phase EXCEPT ![c] = "aborted"]
               /\ res' = [res EXCEPT ![c] = r]
               /\ reg' = reg \ {c}
               /\ blamed' = blamed \cup Blame(r)
               /\ UNCHANGED <<place, admOpen, owned, ownOpenAt, edgeWhenRun, eV, wait, slots>>
          ELSE /\ Admit(c, t, kind[c] = "link", kind[c] = "monitor")
               /\ UNCHANGED <<reg, blamed>>
    /\ UNCHANGED <<pV, oV, readyOpen, xV, commit, commit0, initR, why, resR, pend, dl,
                   nV, mV, seen, dV>>

\* Any number of lookups; a repeated one is a stuttering step.
Lookup ==
    /\ \E c \in reg : seen' = [seen EXCEPT ![c] = TRUE]
    /\ UNCHANGED <<pV, oV, sV, eV, xV, cV, nV, mV, reg, blamed, slots, dV>>

-----------------------------------------------------------------------------
(* Stage 2, on the child. *)

Exit(c, r) ==
    /\ phase' = [phase EXCEPT ![c] = "exiting"]
    /\ why' = [why EXCEPT ![c] = r]

\* A ready child's own exit: a kill that already arrived wins (L3).
ExitK(c, r) == Exit(c, IF killed[c] \/ skill[c] \/ xkill[c] THEN "Killed" ELSE r)

\* A non-ready resolution and D are one critical section (design 2.3): no
\* stopped() runs, so no signal can arrive in between and the requested
\* reason is final. SplitResolutionD (negative control) reopens the window.
DieEffects(c) ==
    /\ lnote' = [lnote EXCEPT ![c] = edge[c] /\ Alive(pst)]
    /\ mnote' = [mnote EXCEPT ![c] = mon[c] /\ Alive(pst)]
    /\ edge' = [edge EXCEPT ![c] = FALSE]
    /\ mon' = [mon EXCEPT ![c] = FALSE]
    /\ reg' = IF Fault = "UnregisterAtDeath" THEN reg \ {c} ELSE reg
    /\ slots' = IF Fault = "EarlySlotRelease" THEN slots \ {c} ELSE slots
ResolveD(c, r) ==
    /\ why' = [why EXCEPT ![c] = r]
    /\ IF Fault = "SplitResolutionD"
       THEN /\ phase' = [phase EXCEPT ![c] = "exiting"]
            /\ UNCHANGED <<lnote, mnote, edge, mon, reg, slots>>
       ELSE /\ phase' = [phase EXCEPT ![c] = "dead"]
            /\ DieEffects(c)

StartedBegin(c) ==
    /\ phase[c] = "starting" /\ ~began[c] /\ ~killed[c] /\ ~skill[c] /\ ~xkill[c]
    /\ began' = [began EXCEPT ![c] = TRUE]
    /\ UNCHANGED <<pV, oV, sV, eV, killed, skill, xkill, astop, pex, qpex, pexAbn, ctrap,
                   cV, nV, mV, rV, dV>>

LateLink(c) ==
    /\ Fault = "LinkAfterStarted"
    /\ kind[c] = "link" /\ began[c] /\ ~linkEver[c]
    /\ phase[c] \in {"starting", "ready"} /\ pst = "run"
    /\ edge' = [edge EXCEPT ![c] = TRUE]
    /\ linkEver' = [linkEver EXCEPT ![c] = TRUE]
    /\ UNCHANGED <<pV, oV, sV, mon, monEver, xV, cV, nV, mV, rV, dV>>

\* An outside kill or an administrative stop (design 2.3 rows 2-3).
OutsideKill(c) ==
    /\ KillEnabled /\ ~xkill[c] /\ phase[c] \in {"starting", "ready", "exiting"}
    /\ xkill' = [xkill EXCEPT ![c] = TRUE]
    \* Before D a kill makes the reason Killed at once (L3).
    /\ why' = IF phase[c] = "exiting" /\ Fault # "SkipEscalation"
              THEN [why EXCEPT ![c] = "Killed"] ELSE why
    /\ UNCHANGED <<pV, oV, sV, eV, began, killed, skill, astop, pex, qpex, pexAbn, ctrap,
                   commit, commit0, initR, wait, res, resR, pend, dl, nV, mV, rV, dV>>
AdminStop(c) ==
    /\ AdminStopEnabled /\ ~astop[c] /\ phase[c] = "starting"
    /\ astop' = [astop EXCEPT ![c] = TRUE]
    /\ UNCHANGED <<pV, oV, sV, eV, began, killed, skill, xkill, pex, qpex, pexAbn, ctrap,
                   cV, nV, mV, rV, dV>>

AnyKill(c) == killed[c] \/ skill[c] \/ xkill[c]

KilledEarly(c) ==
    /\ phase[c] = "starting" /\ ~began[c] /\ AnyKill(c)
    /\ ResolveD(c, "Killed")
    /\ initR' = [initR EXCEPT ![c] = "Killed"]
    /\ IF commit[c] = None THEN SetCommit(c, "failed") ELSE UNCHANGED <<commit, commit0>>
    /\ UNCHANGED <<pV, oV, place, admOpen, owned, ownOpenAt, readyOpen, edgeWhenRun,
                   linkEver, monEver, xV, wait, res, resR, pend, dl,
                   pubAlive, div, divEver, handed, lost, mV, seen, blamed, dV>>

\* started() returns; design 2.3 precedence. No failed row runs stopped().
StartedEnd(c) ==
    /\ phase[c] = "starting" /\ began[c]
    /\ \E o \in Outcomes, t \in (IF ChildCanTrap THEN BOOLEAN ELSE {FALSE}),
          first \in {"admin", "signal"}:
       LET sig == pex[c] \in NonNormal /\ ~t /\ Fault # "DropHeld"
           row3 == astop[c] \/ sig
           r3 == IF astop[c] /\ (~sig \/ first = "admin") THEN "Shutdown" ELSE pex[c]
           closed == (place[c] = "G" /\ ~gopen) \/ (owned[c] /\ ~ownerOpen)
           failWith(r) == /\ ResolveD(c, r)
                          /\ initR' = [initR EXCEPT ![c] = r]
                          /\ IF commit[c] = None THEN SetCommit(c, "failed")
                             ELSE UNCHANGED <<commit, commit0>>
                          /\ UNCHANGED <<cont, readyOpen, qpex>>
       IN /\ ctrap' = [ctrap EXCEPT ![c] = t]
          /\ pex' = [pex EXCEPT ![c] = None]
          /\ IF AnyKill(c) THEN failWith("Killed")
             ELSE IF row3 THEN failWith(r3)
             ELSE IF o = "panic" THEN failWith("Panic")
             ELSE IF o = "stopS" THEN failWith("Shutdown")
             ELSE IF o \in {"stopN", "error"} THEN failWith("Normal")
             ELSE IF o = "ignore"
             THEN /\ ResolveD(c, "Normal")
                  /\ initR' = [initR EXCEPT ![c] = "Normal"]
                  /\ SetCommit(c, "ignored")
                  /\ UNCHANGED <<cont, readyOpen, qpex>>
             ELSE IF closed /\ Fault # "ReadyAfterClose"
             THEN failWith("Shutdown")
             ELSE /\ phase' = [phase EXCEPT ![c] = "ready"]
                  /\ SetCommit(c, "ready")
                  /\ readyOpen' = [readyOpen EXCEPT ![c] = ~closed]
                  /\ cont' = [cont EXCEPT ![c] = o = "continue"]
                  /\ qpex' = [qpex EXCEPT ![c] = IF t THEN pex[c] ELSE None]
                  /\ UNCHANGED <<why, initR, lnote, mnote, edge, mon, reg, slots>>
    /\ UNCHANGED <<pV, oV, place, admOpen, owned, ownOpenAt, edgeWhenRun, linkEver, monEver,
                   began, killed, skill, xkill, astop, pexAbn,
                   wait, res, resR, pend, dl, pubAlive, div, divEver, handed, lost,
                   msgs, early, seen, blamed, dV>>

Send(c) ==
    /\ phase[c] \in {"starting", "ready"} /\ msgs[c] < MaxMsgs
    /\ msgs' = [msgs EXCEPT ![c] = msgs[c] + 1]
    /\ UNCHANGED <<pV, oV, sV, eV, xV, cV, nV, cont, early, rV, dV>>

RunContinue(c) ==
    /\ phase[c] = "ready" /\ cont[c]
    /\ cont' = [cont EXCEPT ![c] = FALSE]
    /\ UNCHANGED <<pV, oV, sV, eV, xV, cV, nV, msgs, early, rV, dV>>

Handle(c) ==
    /\ phase[c] = "ready" /\ msgs[c] > 0
    /\ ~cont[c] \/ Fault = "ContinueQueued"
    /\ msgs' = [msgs EXCEPT ![c] = msgs[c] - 1]
    /\ early' = [early EXCEPT ![c] = early[c] \/ cont[c]]
    /\ UNCHANGED <<pV, oV, sV, eV, xV, cV, nV, cont, rV, dV>>

HandleQueuedExit(c) ==
    /\ phase[c] = "ready" /\ qpex[c] # None
    /\ ~cont[c] \/ Fault = "ContinueQueued"
    /\ early' = [early EXCEPT ![c] = early[c] \/ cont[c]]
    /\ qpex' = [qpex EXCEPT ![c] = None]
    /\ ExitK(c, qpex[c])
    /\ UNCHANGED <<pV, oV, place, admOpen, owned, ownOpenAt, readyOpen, edgeWhenRun, eV,
                   began, killed, skill, xkill, astop, pex, pexAbn, ctrap,
                   commit, commit0, initR, wait, res, resR, pend, dl, nV, cont, msgs, rV, dV>>

ApplyParentExit(c) ==
    /\ phase[c] = "ready" /\ pex[c] # None
    /\ pex' = [pex EXCEPT ![c] = None]
    /\ IF pex[c] \in NonNormal THEN ExitK(c, pex[c]) ELSE UNCHANGED <<phase, why>>
    /\ UNCHANGED <<pV, oV, place, admOpen, owned, ownOpenAt, readyOpen, edgeWhenRun, eV,
                   began, killed, skill, xkill, astop, qpex, pexAbn, ctrap,
                   commit, commit0, initR, wait, res, resR, pend, dl, nV, mV, rV, dV>>

ReadyExit(c) ==
    /\ phase[c] = "ready"
    /\ \E r \in {"Normal", "Panic"} : ExitK(c, r)
    /\ UNCHANGED <<pV, oV, place, admOpen, owned, ownOpenAt, readyOpen, edgeWhenRun, eV,
                   xV, commit, commit0, initR, wait, res, resR, pend, dl, nV, mV, rV, dV>>

KillReady(c) ==
    /\ phase[c] = "ready" /\ AnyKill(c)
    /\ Exit(c, "Killed")
    /\ UNCHANGED <<pV, oV, place, admOpen, owned, ownOpenAt, readyOpen, edgeWhenRun, eV,
                   xV, commit, commit0, initR, wait, res, resR, pend, dl, nV, mV, rV, dV>>

ChildUnlink(c) ==
    /\ UnlinkEnabled /\ phase[c] = "ready" /\ edge[c]
    /\ edge' = [edge EXCEPT ![c] = FALSE]
    /\ UNCHANGED <<pV, oV, sV, mon, linkEver, monEver, xV, cV, nV, mV, rV, dV>>

\* D: the edges close and their notices are taken; the reason is final.
Die(c) ==
    /\ phase[c] = "exiting"
    /\ phase' = [phase EXCEPT ![c] = "dead"]
    /\ DieEffects(c)
    /\ UNCHANGED <<pV, oV, place, admOpen, owned, ownOpenAt, readyOpen, edgeWhenRun,
                   linkEver, monEver, xV, cV, pubAlive, div, divEver, handed, lost, mV,
                   seen, blamed, dV>>

\* After destruction: the name is released (registry lock), then the slot
\* (group lock); two atomic points, two trace events (design 5.2, 16.2).
ReleaseName(c) ==
    /\ phase[c] = "dead"
    /\ phase' = [phase EXCEPT ![c] = "namerel"]
    /\ reg' = IF Fault = "LateUnregister" THEN reg ELSE reg \ {c}
    /\ UNCHANGED <<pV, oV, place, admOpen, owned, ownOpenAt, readyOpen, edgeWhenRun,
                   eV, xV, cV, nV, mV, seen, blamed, slots, dV>>
ReleaseSlot(c) ==
    /\ phase[c] = "namerel"
    /\ phase' = [phase EXCEPT ![c] = "released"]
    /\ slots' = slots \ {c}
    /\ UNCHANGED <<pV, oV, place, admOpen, owned, ownOpenAt, readyOpen, edgeWhenRun,
                   eV, xV, cV, nV, mV, reg, seen, blamed, dV>>

\* Publication: each taken notice is disposed of by the commitment and the
\* waiter (design 5.2); answers become final.
Publish(c) ==
    /\ phase[c] = "released"
    /\ phase' = [phase EXCEPT ![c] = "published"]
    /\ pubAlive' = [pubAlive EXCEPT ![c] = Alive(pst)]
    /\ LET r == why[c]
           fs == commit0[c] \in FailCommits
                 \/ (Fault = "MarkKeptAfterReady" /\ commit0[c] = "ready")
           ln == lnote[c] /\ Alive(pst)
           mn == mnote[c] /\ Alive(pst)
           w == wait[c]
           ordinary == ~fs \/ w \in {"none", "abandoned"}
                       \/ (Fault = "KillBeforeUnlink" /\ commit0[c] = "timedout")
           divertL == ln /\ ~ordinary /\ w = "waiting"
                      /\ (ptrap \/ commit0[c] = "timedout")
           divertM == mn /\ ~ordinary /\ w = "waiting"
           applyNow == ln /\ ~ordinary /\ ~ptrap /\ commit0[c] # "timedout"
                       /\ (r # "Normal" \/ Fault = "KillOnNormalFailure")
                       /\ Fault # "NoParentKillOnFailure"
           dbl == Fault = "DoubleNotice" /\ divertL /\ ptrap
           b1 == ln /\ (ordinary \/ (ptrap /\ Fault = "NoLinkFlush") \/ dbl)
           b2 == ~b1 /\ applyNow /\ pst = "run"
           b3 == ~b1 /\ ~b2 /\ mn /\ (ordinary \/ Fault = "NoMonitorFlush")
           legit == (commit0[c] \in FailCommits /\ w = "consumed")
                    \/ (ln /\ ~ptrap /\ (r = "Normal" \/ pst # "run"))
       IN /\ lost' = [lost EXCEPT ![c] = lost[c]
                         \/ ((ln \/ mn) /\ ~(b1 \/ b2 \/ b3 \/ divertL \/ divertM) /\ ~legit)]
          /\ div' = IF divertL \/ divertM THEN [div EXCEPT ![c] = r] ELSE div
          /\ divEver' = IF divertL \/ divertM THEN [divEver EXCEPT ![c] = TRUE] ELSE divEver
          \* The disposition is decided here; delivery is a later step.
          /\ obN' = IF b1 \/ b2 \/ b3 THEN [obN EXCEPT ![c] = @ + 1] ELSE obN
          /\ obR' = IF b1 \/ b2 \/ b3 THEN [obR EXCEPT ![c] = r] ELSE obR
    /\ UNCHANGED <<pV, oV, place, admOpen, owned, ownOpenAt, readyOpen, edgeWhenRun,
                   eV, xV, cV, lnote, mnote, handed, mV, rV,
                   exitN, downN, exitR, downR, dlvOk>>

\* Delivery of a disposed notice to P (its mailbox, or the signal applied
\* to a non-trapping P); a separate event (PNotice) from the disposition.
Deliver(c) ==
    /\ obN[c] > 0
    /\ obN' = [obN EXCEPT ![c] = @ - 1]
    /\ dlvOk' = [dlvOk EXCEPT ![c] = Alive(pst)]
    /\ LET r == obR[c]
           force == Fault = "KillOnNormalFailure" /\ ~ptrap /\ kind[c] = "link"
                    /\ commit0[c] \in {"failed", "ignored"} /\ pst = "run"
       IN IF kind[c] = "monitor"
          THEN DeliverDown(c, r) /\ UNCHANGED <<exitN, exitR, pst, pwhy, pcause>>
          ELSE IF force
          THEN /\ pst' = "exiting" /\ pwhy' = r /\ pcause' = c
               /\ UNCHANGED <<exitN, exitR, downN, downR>>
          ELSE DeliverLink(c, r) /\ UNCHANGED <<downN, downR>>
    /\ UNCHANGED <<pplace, ptrap, gopen, ownerOpen, gdone, sfired, pn, oV, sV, eV, xV,
                   cV, nV, mV, rV, obR>>

LateRelease(c) ==
    /\ Fault = "LateUnregister" /\ phase[c] = "published" /\ c \in reg
    /\ reg' = reg \ {c}
    /\ UNCHANGED <<pV, oV, sV, eV, xV, cV, nV, mV, seen, blamed, slots, dV>>

-----------------------------------------------------------------------------
(* The init timer, the cleanup deadline and the waiter. *)

\* The framework-owned init timer commits timedout only while nothing is
\* committed. RewriteCommit / TimerIgnoresCommit are negative controls.
Timeout(c) ==
    /\ TimeoutEnabled /\ kind[c] # None /\ ~killed[c]
    /\ \/ commit[c] = None /\ phase[c] = "starting"
       \/ Fault \in {"TimerIgnoresCommit", "RewriteCommit"} /\ phase[c] \in {"starting", "ready"}
    /\ IF commit[c] = None \/ Fault = "RewriteCommit"
       THEN SetCommit(c, "timedout") ELSE UNCHANGED <<commit, commit0>>
    /\ killed' = [killed EXCEPT ![c] = TRUE]
    /\ UNCHANGED <<pV, oV, sV, eV, began, skill, xkill, astop, pex, qpex, pexAbn, ctrap,
                   initR, why, wait, res, resR, pend, dl, nV, mV, rV, dV>>

\* The bounded wait for publication expires (measured from the commitment).
CleanupDeadline(c) ==
    /\ commit0[c] \in FailCommits /\ ~dl[c]
    /\ phase[c] \in {"starting", "exiting", "dead", "namerel"}
    /\ dl' = [dl EXCEPT ![c] = TRUE]
    /\ UNCHANGED <<pV, oV, sV, eV, xV, commit, commit0, initR, why, wait, res, resR, pend,
                   nV, mV, rV, dV>>

AckOk(c) ==
    /\ wait[c] = "waiting"
    /\ \/ commit0[c] = "ready"
       \/ Fault = "AckBeforeStarted" /\ commit[c] = None /\ phase[c] = "starting"
       \/ Fault = "ErrorAsOk" /\ commit0[c] = "ignored"
    /\ res' = [res EXCEPT ![c] = "ok"]
    /\ wait' = [wait EXCEPT ![c] = "consumed"]
    /\ div' = [div EXCEPT ![c] = None]
    /\ lost' = [lost EXCEPT ![c] = lost[c] \/ div[c] # None]
    /\ UNCHANGED <<pV, oV, sV, eV, xV, commit, commit0, initR, why, resR, pend, dl,
                   lnote, mnote, pubAlive, divEver, handed, mV, rV, dV>>

\* An error or ignore answers after publication (final reason), or after the
\* cleanup deadline with the cleanup pending (provisional reason).
AckFail(c) ==
    /\ wait[c] = "waiting"
    /\ \/ commit0[c] \in FailCommits
       \/ Fault = "AnswerReadyAsFailure" /\ commit0[c] = "ready" /\ phase[c] = "published"
    /\ ~(Fault = "NoAnswerIgnore" /\ commit0[c] = "ignored")
    /\ \E late \in BOOLEAN :
         /\ IF late THEN phase[c] \in {"starting", "exiting", "dead", "namerel", "released"}
                         /\ (dl[c] \/ Fault = "EarlyPending")
            ELSE \/ phase[c] = "published"
                 \/ Fault = "EarlyFailAck" /\ phase[c] = "dead"
         /\ pend' = [pend EXCEPT ![c] = late]
         /\ resR' = [resR EXCEPT ![c] = IF why[c] = None THEN "Killed" ELSE why[c]]
    /\ res' = [res EXCEPT ![c] = CASE commit0[c] = "ignored" -> "ignored"
                                   [] commit0[c] = "timedout" -> "timeout"
                                   [] OTHER -> "failed"]
    /\ wait' = [wait EXCEPT ![c] = "consumed"]
    /\ div' = [div EXCEPT ![c] = None]
    /\ UNCHANGED <<pV, oV, sV, eV, xV, commit, commit0, initR, why, dl,
                   lnote, mnote, pubAlive, divEver, handed, lost, mV, rV, dV>>

\* The Starting is dropped: a diverted notice goes to P as an ordinary one.
Abandon(c) ==
    /\ wait[c] = "waiting"
    /\ wait' = [wait EXCEPT ![c] = "abandoned"]
    /\ div' = [div EXCEPT ![c] = None]
    /\ LET r == div[c]
           give == r # None /\ Alive(pst)
           drop == Fault = "AbandonDropsNotice"
       IN /\ handed' = [handed EXCEPT ![c] = give]
          /\ lost' = [lost EXCEPT ![c] = lost[c] \/ (give /\ drop)]
          /\ obN' = IF give /\ ~drop THEN [obN EXCEPT ![c] = @ + 1] ELSE obN
          /\ obR' = IF give /\ ~drop THEN [obR EXCEPT ![c] = r] ELSE obR
    /\ UNCHANGED <<pV, oV, sV, eV, xV,
                   commit, commit0, initR, why, res, resR, pend, dl,
                   lnote, mnote, pubAlive, divEver, mV, rV,
                   exitN, downN, exitR, downR, dlvOk>>

-----------------------------------------------------------------------------
(* The parent, its scope and the groups. *)

PExit ==
    /\ ParentCanExit /\ pst = "run"
    /\ \E r \in {"Normal", "Panic"} : pst' = "exiting" /\ pwhy' = r
    /\ UNCHANGED <<pplace, ptrap, pcause, gopen, ownerOpen, gdone, sfired, pn,
                   oV, sV, eV, xV, cV, nV, mV, rV, dV>>

OwnerClose ==
    /\ OwnerCloseEnabled /\ ownerOpen
    /\ ownerOpen' = FALSE
    /\ UNCHANGED <<pst, pplace, ptrap, pwhy, pcause, gopen, gdone, sfired, pn,
                   oV, sV, eV, xV, cV, nV, mV, rV, dV>>

\* P's D: its link notices are taken; edges and monitors close.
PDie ==
    /\ pst = "exiting"
    /\ pst' = "dead"
    /\ pn' = [c \in Kids |-> IF edge[c] THEN pwhy ELSE pn[c]]
    /\ edge' = F(FALSE)
    /\ mon' = F(FALSE)
    /\ UNCHANGED <<pplace, ptrap, pwhy, pcause, gopen, ownerOpen, gdone, sfired,
                   oV, sV, linkEver, monEver, xV, cV, nV, mV, rV, dV>>

\* P's publication: the parent exits reach the linked children, and the
\* scope sends every live owned child a polite Shutdown (S8; E3 parent
\* signal); P's slot is released.
PPublish ==
    /\ pst = "dead"
    /\ pst' = "published"
    /\ LET sc(c) == ScopeEnabled /\ owned[c] /\ phase[c] \in {"starting", "ready"}
                    /\ Fault # "NoPoliteStop"
           rr(c) == IF pn[c] \in NonNormal THEN pn[c]
                    ELSE IF sc(c) THEN "Shutdown" ELSE pn[c]
           trapReady(c) == phase[c] = "ready" /\ ctrap[c]
       IN /\ pex' = [c \in Kids |-> IF rr(c) # None /\ phase[c] \in {"starting", "ready"}
                                       /\ ~trapReady(c) THEN rr(c) ELSE pex[c]]
          /\ qpex' = [c \in Kids |-> IF rr(c) # None /\ trapReady(c) THEN rr(c) ELSE qpex[c]]
          /\ pexAbn' = [c \in Kids |-> pexAbn[c] \/ (rr(c) \in NonNormal
                                        /\ phase[c] \in {"starting", "ready"})]
    /\ pn' = F(None)
    /\ UNCHANGED <<pplace, ptrap, pwhy, pcause, gopen, ownerOpen, gdone, sfired,
                   oV, sV, eV, began, killed, skill, xkill, astop, ctrap, cV, nV, mV, rV, dV>>

\* S8: from P's exit begin, the scope deadline (a timer, not assumed fair)
\* kills every live owned child directly, exiting ones included.
ScopeDeadline ==
    /\ ScopeEnabled /\ pplace = "G" /\ pst # "run" /\ ~sfired
    /\ sfired' = TRUE
    /\ skill' = [c \in Kids |-> skill[c] \/ (owned[c]
                    /\ phase[c] \in (IF Fault = "NoScopeKill" THEN {"starting", "ready"}
                                     ELSE {"starting", "ready", "exiting"}))]
    /\ why' = [c \in Kids |-> IF owned[c] /\ phase[c] = "exiting" /\ Fault \notin {"NoScopeKill", "SkipEscalation"}
                             THEN "Killed" ELSE why[c]]
    /\ UNCHANGED <<pst, pplace, ptrap, pwhy, pcause, gopen, ownerOpen, gdone, pn,
                   oV, sV, eV, began, killed, xkill, astop, pex, qpex, pexAbn, ctrap,
                   commit, commit0, initR, wait, res, resR, pend, dl, nV, mV, rV, dV>>

\* P's generation is done: P published and every owned child published.
GenDone ==
    /\ ScopeEnabled /\ pplace = "G" /\ ~gdone
    /\ pst = "published" \/ (Fault = "GenDoneAtParentD" /\ pst = "dead")
    /\ \A c \in Kids : owned[c] => phase[c] \in {"published", "aborted"}
                                   \/ Fault = "EarlyGenDone"
    /\ gdone' = TRUE
    /\ UNCHANGED <<pst, pplace, ptrap, pwhy, pcause, gopen, ownerOpen, sfired, pn,
                   oV, sV, eV, xV, cV, nV, mV, rV, dV>>

GClose ==
    /\ CloseEnabled /\ gopen
    /\ gopen' = FALSE
    /\ UNCHANGED <<pst, pplace, ptrap, pwhy, pcause, ownerOpen, gdone, sfired, pn,
                   oV, sV, eV, xV, cV, nV, mV, rV, dV>>

ChildStep(c) ==
    \/ Begin(c) \/ PrepareFail(c) \/ Commit(c) \/ RegisterFirst(c) \/ CommitRest(c)
    \/ StartedBegin(c) \/ LateLink(c) \/ OutsideKill(c) \/ AdminStop(c)
    \/ KilledEarly(c) \/ StartedEnd(c)
    \/ Send(c) \/ RunContinue(c) \/ Handle(c) \/ HandleQueuedExit(c)
    \/ ApplyParentExit(c) \/ ReadyExit(c) \/ KillReady(c)
    \/ ChildUnlink(c) \/ Die(c) \/ ReleaseName(c) \/ ReleaseSlot(c) \/ Publish(c) \/ Deliver(c)
    \/ LateRelease(c)
    \/ Timeout(c) \/ CleanupDeadline(c) \/ AckOk(c) \/ AckFail(c) \/ Abandon(c)

Next == (\E c \in Kids : ChildStep(c)) \/ Lookup \/ PExit \/ OwnerClose \/ PDie
        \/ PPublish \/ ScopeDeadline \/ GenDone \/ GClose

\* Liveness assumes the child's own steps, its death and publication, the
\* waiter's answer, and P's D and publication complete. The scope deadline
\* and the cleanup deadline are not assumed to fire.
Progress(c) ==
    \/ StartedBegin(c) \/ KilledEarly(c) \/ StartedEnd(c) \/ RunContinue(c)
    \/ HandleQueuedExit(c) \/ ApplyParentExit(c) \/ KillReady(c)
    \/ Die(c) \/ ReleaseName(c) \/ ReleaseSlot(c) \/ Publish(c) \/ Deliver(c) \/ AckOk(c)
Fairness ==
    /\ \A c \in Kids : /\ WF_vars(Progress(c)) /\ WF_vars(Commit(c))
                       /\ SF_vars(AckFail(c))
    /\ WF_vars(PDie) /\ WF_vars(PPublish) /\ WF_vars(GenDone)

Spec == Init /\ [][Next]_vars
LiveSpec == Spec /\ Fairness

-----------------------------------------------------------------------------
(* Safety. *)

InheritedPlacement == \A c \in Kids : place[c] # None => place[c] = Expected(c)

LinkBeforeStarted ==
    \A c \in Kids :
        /\ (began[c] /\ kind[c] = "link") => linkEver[c]
        /\ (began[c] /\ kind[c] = "monitor") => monEver[c]

\* An edge is committed only while P runs (not after its exit began).
EdgeOnlyToRunningParent == \A c \in Kids : (linkEver[c] \/ monEver[c]) => edgeWhenRun[c]
NoEdgeToDeadParent == \A c \in Kids : (edge[c] \/ mon[c]) => Alive(pst)

NameUnique == Cardinality(reg) <= 1
\* Every holder is between its commit and its release ...
NameHeldUntilReleased == \A c \in reg : phase[c] \in {"reg1"} \cup LiveP
\* ... and an admitted named child keeps the name until its release.
NameKeptWhileLive == \A c \in Kids : (wname[c] /\ phase[c] \in LiveP) => c \in reg
\* A new holder never overlaps an old holder that is not yet released.
NoOverlapOnReuse ==
    \A c \in reg : \A d \in Kids \ {c} : wname[d] => phase[d] \notin LiveP
\* No notice, parent kill or final answer about c while c holds the name.
NameFreeBeforeObservation ==
    \A c \in Kids :
        (exitN[c] + downN[c] > 0 \/ pcause = c
         \/ (res[c] \in {"failed", "ignored", "timeout"} /\ ~pend[c]))
            => c \notin reg

\* A refused start is never observable, also not through a competitor's
\* refusal.
AbortInvisible ==
    \A c \in Kids : phase[c] = "aborted" =>
        /\ ~edge[c] /\ ~mon[c] /\ ~linkEver[c] /\ ~monEver[c] /\ c \notin reg
        /\ ~seen[c] /\ c \notin blamed /\ c \notin slots /\ ~began[c] /\ place[c] = None
        /\ exitN[c] = 0 /\ downN[c] = 0 /\ pcause # c

\* The commitment never changes once made.
CommitImmutable == \A c \in Kids : commit0[c] # None => commit[c] = commit0[c]

\* The answer is the first commitment.
ResultMatchesCommit ==
    \A c \in Kids :
        /\ res[c] = "ok" /\ Waiting(kind[c]) => commit0[c] = "ready"
        /\ res[c] = "failed" => commit0[c] = "failed"
        /\ res[c] = "ignored" => commit0[c] = "ignored"
        /\ res[c] = "timeout" => commit0[c] = "timedout"

ReadyIsFinal == \A c \in Kids : commit0[c] = "ready" => res[c] \notin {"failed", "ignored", "timeout"}

TimeoutNeverKillsReady == \A c \in Kids : commit0[c] = "ready" => ~killed[c]

ReadyOnlyWhileOpen == \A c \in Kids : commit0[c] = "ready" => readyOpen[c]

\* An answer without pending comes after publication and reports the final
\* exit reason; a pending answer only after the cleanup deadline.
FailureAfterPublication ==
    \A c \in Kids : (res[c] \in {"failed", "ignored", "timeout"} /\ ~pend[c]) => phase[c] = "published"
ReportedReasonFinal ==
    \A c \in Kids : (res[c] = "failed" /\ ~pend[c]) => resR[c] = why[c]
PendingOnlyAfterDeadline == \A c \in Kids : pend[c] => dl[c]

\* A kill that arrived before D makes the final reason Killed.
KillBeforeDIsKilled ==
    \A c \in Kids : ((killed[c] \/ skill[c] \/ xkill[c]) /\ phase[c] \in {"dead", "released", "published"})
        => why[c] = "Killed"
\* An ignored start ends Normal: no later signal can change it (the
\* resolution and D are one step).
IgnoredExitsNormal ==
    \A c \in Kids : (commit0[c] = "ignored" /\ phase[c] \in {"dead", "released", "published"})
        => why[c] = "Normal"

\* A failed start's notice is consumed: no Down; no Exit to a trapping P.
FailureFlushed ==
    \A c \in Kids : res[c] \in {"failed", "ignored", "timeout"} =>
        /\ kind[c] = "monitor" => downN[c] = 0
        /\ (kind[c] = "link" /\ ptrap) => exitN[c] = 0
\* ... but a non-Normal init failure still ends a non-trapping P (OTP) ...
FailureEndsNonTrappingParent ==
    \A c \in Kids :
        (kind[c] = "link" /\ ~ptrap /\ commit0[c] = "failed" /\ why[c] \in NonNormal
         /\ phase[c] = "published" /\ lnote[c] /\ pubAlive[c] /\ obN[c] = 0) => pst # "run"
\* ... and one whose final reason is Normal (error, ignore, stop(Normal),
\* not escalated by a kill before D) never does.
NormalFailureSparesParent ==
    \A c \in Kids :
        (kind[c] = "link" /\ commit0[c] \in {"failed", "ignored"} /\ why[c] = "Normal")
            => pcause # c

TimeoutContained ==
    \A c \in Kids : (commit0[c] = "timedout" /\ wait[c] # "abandoned") =>
        /\ pcause # c /\ exitN[c] = 0 /\ downN[c] = 0

AdmittedOnlyOpen == \A c \in Kids : place[c] = "G" => admOpen[c]
OwnedOnlyWhileOwnerOpen == \A c \in Kids : owned[c] => ownOpenAt[c]

\* Slots (P's included) are held until release, and never exceed Cap.
CapacityRespected == Used <= Cap
ParentSlotHeld == (pplace = "G" /\ pst \in {"run", "exiting", "dead"}) => PSlot = 1
SlotHeldUntilReleased == \A c \in Kids : (place[c] = "G" /\ phase[c] \in LiveP \cup {"namerel"}) => c \in slots

NoticeOnce == \A c \in Kids : exitN[c] <= 1 /\ downN[c] <= 1

\* No notice owed to P is ever dropped.
NoLostNotice == \A c \in Kids : ~lost[c]

\* A notice handed on by an abandoned wait reaches P with its kind and reason.
AbandonedNoticeDelivered ==
    \A c \in Kids : (handed[c] /\ obN[c] = 0 /\ dlvOk[c]) =>
        /\ (kind[c] = "link" /\ ptrap) => exitN[c] >= 1 /\ exitR[c] = why[c]
        /\ (kind[c] = "link" /\ ~ptrap /\ why[c] \in NonNormal) => pst # "run"
        /\ kind[c] = "monitor" => downN[c] >= 1 /\ downR[c] = why[c]

ContinueFirst == \A c \in Kids : ~early[c]

GenDoneMeansOwnedDone ==
    gdone => /\ pst = "published"
             /\ \A c \in Kids : owned[c] => phase[c] \in {"published", "aborted"}

\* When the scope deadline fires, every live owned child is killed.
DeadlineKillsAllOwned ==
    sfired => \A c \in Kids : (owned[c] /\ phase[c] \in {"starting", "ready", "exiting"})
                              => skill[c]

(* Liveness. *)
ParentExitReachesChild ==
    \A c \in Kids : pexAbn[c] ~> phase[c] \in {"published", "aborted"}
StartAnswered == \A c \in Kids : wait[c] = "waiting" ~> wait[c] # "waiting"
\* Once P is published, its generation ends without the scope deadline
\* (the polite Shutdown suffices when children cooperate).
OwnedScopeFinishes == (ScopeEnabled /\ pplace = "G" /\ pst = "published") ~> gdone
=============================================================================
