------------------------------- MODULE Start -------------------------------
EXTENDS Naturals, FiniteSets
(***************************************************************************)
(* Unified start API, revision 2 (0.9; design docs/design/unified-start.md).*)
(*                                                                         *)
(* One parent actor P (managed in group G or unmanaged; it may trap exits  *)
(* and exit) starts children with spawn, start, start_link or              *)
(* start_monitor, each with an explicit or default placement and possibly  *)
(* the one shared name.                                                    *)
(*                                                                         *)
(* Stage 1 is one commit (design section 3): placement admission, the      *)
(* parent's state, the link or monitor edge and the name are decided under *)
(* the commit locks together, so a refused start is never observable. A    *)
(* dispatch failure happens before the commit (PrepareFail).               *)
(*                                                                         *)
(* Every waiting start owns a startup cell (design section 5): one         *)
(* terminal commitment (ready, failed, ignored, timedout) made exactly     *)
(* once, by the child at the end of started() or by the init timer, and a  *)
(* waiter state (waiting, abandoned, consumed). Readiness clears the start *)
(* marks on the edges, so a later death is an ordinary death. A failed     *)
(* start's notice is diverted to the waiter at publication and consumed by *)
(* the error, or handed to P if the wait was abandoned.                    *)
(*                                                                         *)
(* Death is three steps, as in otp-exit-signals: D (Die: edges closed and  *)
(* their notices taken), destruction, publication (Publish: name and slot  *)
(* released, then notices, then the waiter). Destruction is folded into    *)
(* the gap between Die and Publish; a waiter may give up at its bounded    *)
(* cleanup deadline (pend = TRUE) before publication.                      *)
(*                                                                         *)
(* A linked child placed in P's group is owned by P's scope (S8): P's      *)
(* generation is done only when the owned children are published; the      *)
(* scope deadline kills owned children that outlive P's exit (unlinked or  *)
(* not).                                                                   *)
(***************************************************************************)
CONSTANTS
    Kids,           \* child names
    Kinds,          \* subset of {"spawn","start","link","monitor"}
    Explicit,       \* subset of {"none","G","U","same"} ("same" = placement_of)
    ParentPlaces,   \* subset of {"G","U"}
    Cap,            \* live actors G admits (P included); held until publication
    Named,          \* children may request the one shared name
    TimeoutEnabled, \* the init timer may fire
    ParentCanExit,  \* P may exit (Normal or Panic)
    CloseEnabled,   \* G may close (system shutdown)
    MaxMsgs,        \* messages others send to each child
    ChildCanTrap,   \* started() may set trap_exit
    UnlinkEnabled,  \* a ready linked child may unlink from P
    ScopeEnabled,   \* model P's generation end and its scope deadline (S8)
    Fault           \* "None" or a negative control (README, Unified start)

None == "none"
NonNormal == {"Panic", "Killed", "Shutdown"}
Reasons == {"Normal"} \cup NonNormal
Phases == {"idle", "begun", "reg1", "starting", "ready", "exiting", "dead",
           "published", "aborted"}
Commits == {None, "ready", "failed", "ignored", "timedout"}
Waits == {"none", "waiting", "abandoned", "consumed"}
Results == {"none", "pending", "ok", "failed", "ignored", "timeout", "closed",
            "capacity", "already", "parentexited", "system"}
Outcomes == {"ready", "continue", "panic", "stop", "error", "ignore"}

VARIABLES
    pst, pplace, ptrap, pwhy, pcause, gopen, gdone,
    kind, expl, wname,
    phase, place, admOpen, owned, readyOpen,
    edge, mon, linkEver, monEver,
    began, killed, skill, pex, qpex, pexAbn, ctrap,
    commit, why, wait, res, pend,
    lnote, mnote, div, divEver,
    cont, msgs, early,
    reg, seen,
    exitN, downN, exitR, downR

pV == <<pst, pplace, ptrap, pwhy, pcause, gopen, gdone>>
oV == <<kind, expl, wname>>
sV == <<phase, place, admOpen, owned, readyOpen>>
eV == <<edge, mon, linkEver, monEver>>
xV == <<began, killed, skill, pex, qpex, pexAbn, ctrap>>
cV == <<commit, why, wait, res, pend>>
nV == <<lnote, mnote, div, divEver>>
mV == <<cont, msgs, early>>
rV == <<reg, seen>>
dV == <<exitN, downN, exitR, downR>>
vars == <<pV, oV, sV, eV, xV, cV, nV, mV, rV, dV>>

Waiting(k) == k \in {"start", "link", "monitor"}
Live(c) == phase[c] \in {"reg1", "starting", "ready", "exiting", "dead"}
LiveInG == Cardinality({c \in Kids : place[c] = "G" /\ Live(c)})
           + (IF pplace = "G" /\ pst # "dead" THEN 1 ELSE 0)

Expected(c) ==
    CASE expl[c] = "same" -> pplace
      [] expl[c] \in {"G", "U"} -> expl[c]
      [] kind[c] \in {"link", "monitor"} -> pplace
      [] OTHER -> "U"
Target(c) ==
    IF expl[c] = None /\ kind[c] \in {"link", "monitor"} /\ Fault = "NoInherit"
    THEN "U" ELSE Expected(c)

TypeOK ==
    /\ pst \in {"run", "exiting", "dead"} /\ pplace \in {"G", "U"}
    /\ ptrap \in BOOLEAN /\ pwhy \in Reasons \cup {None}
    /\ pcause \in Kids \cup {None} /\ gopen \in BOOLEAN /\ gdone \in BOOLEAN
    /\ kind \in [Kids -> {"spawn", "start", "link", "monitor", None}]
    /\ expl \in [Kids -> {"G", "U", "same", None}]
    /\ wname \in [Kids -> BOOLEAN]
    /\ phase \in [Kids -> Phases] /\ place \in [Kids -> {"G", "U", None}]
    /\ admOpen \in [Kids -> BOOLEAN] /\ owned \in [Kids -> BOOLEAN]
    /\ readyOpen \in [Kids -> BOOLEAN]
    /\ edge \in [Kids -> BOOLEAN] /\ mon \in [Kids -> BOOLEAN]
    /\ linkEver \in [Kids -> BOOLEAN] /\ monEver \in [Kids -> BOOLEAN]
    /\ began \in [Kids -> BOOLEAN] /\ killed \in [Kids -> BOOLEAN]
    /\ skill \in [Kids -> BOOLEAN]
    /\ pex \in [Kids -> Reasons \cup {None}] /\ qpex \in [Kids -> Reasons \cup {None}]
    /\ pexAbn \in [Kids -> BOOLEAN] /\ ctrap \in [Kids -> BOOLEAN]
    /\ commit \in [Kids -> Commits] /\ why \in [Kids -> Reasons \cup {None}]
    /\ wait \in [Kids -> Waits] /\ res \in [Kids -> Results]
    /\ pend \in [Kids -> BOOLEAN]
    /\ lnote \in [Kids -> BOOLEAN] /\ mnote \in [Kids -> BOOLEAN]
    /\ div \in [Kids -> Reasons \cup {None}] /\ divEver \in [Kids -> BOOLEAN]
    /\ cont \in [Kids -> BOOLEAN] /\ msgs \in [Kids -> 0..MaxMsgs]
    /\ early \in [Kids -> BOOLEAN]
    /\ reg \subseteq Kids /\ seen \in [Kids -> BOOLEAN]
    /\ exitN \in [Kids -> 0..3] /\ downN \in [Kids -> 0..3]
    /\ exitR \in [Kids -> Reasons \cup {None}] /\ downR \in [Kids -> Reasons \cup {None}]

Init ==
    /\ pst = "run" /\ pplace \in ParentPlaces /\ ptrap \in BOOLEAN
    /\ pwhy = None /\ pcause = None /\ gopen = TRUE /\ gdone = FALSE
    /\ kind = [c \in Kids |-> None] /\ expl = [c \in Kids |-> None]
    /\ wname = [c \in Kids |-> FALSE]
    /\ phase = [c \in Kids |-> "idle"] /\ place = [c \in Kids |-> None]
    /\ admOpen = [c \in Kids |-> FALSE] /\ owned = [c \in Kids |-> FALSE]
    /\ readyOpen = [c \in Kids |-> FALSE]
    /\ edge = [c \in Kids |-> FALSE] /\ mon = [c \in Kids |-> FALSE]
    /\ linkEver = [c \in Kids |-> FALSE] /\ monEver = [c \in Kids |-> FALSE]
    /\ began = [c \in Kids |-> FALSE] /\ killed = [c \in Kids |-> FALSE]
    /\ skill = [c \in Kids |-> FALSE]
    /\ pex = [c \in Kids |-> None] /\ qpex = [c \in Kids |-> None]
    /\ pexAbn = [c \in Kids |-> FALSE] /\ ctrap = [c \in Kids |-> FALSE]
    /\ commit = [c \in Kids |-> None] /\ why = [c \in Kids |-> None]
    /\ wait = [c \in Kids |-> "none"] /\ res = [c \in Kids |-> "none"]
    /\ pend = [c \in Kids |-> FALSE]
    /\ lnote = [c \in Kids |-> FALSE] /\ mnote = [c \in Kids |-> FALSE]
    /\ div = [c \in Kids |-> None] /\ divEver = [c \in Kids |-> FALSE]
    /\ cont = [c \in Kids |-> FALSE] /\ msgs = [c \in Kids |-> 0]
    /\ early = [c \in Kids |-> FALSE]
    /\ reg = {} /\ seen = [c \in Kids |-> FALSE]
    /\ exitN = [c \in Kids |-> 0] /\ downN = [c \in Kids |-> 0]
    /\ exitR = [c \in Kids |-> None] /\ downR = [c \in Kids |-> None]

-----------------------------------------------------------------------------
(* Notices reaching P. *)

\* A link notice: a trapping P gets an Exit; a non-trapping running P exits
\* on a non-Normal reason.
DeliverLink(c, r) ==
    IF ptrap
    THEN /\ exitN' = [exitN EXCEPT ![c] = @ + 1]
         /\ exitR' = [exitR EXCEPT ![c] = r]
         /\ UNCHANGED <<pst, pwhy, pcause>>
    ELSE IF r # "Normal" /\ pst = "run"
    THEN /\ pst' = "exiting" /\ pwhy' = r /\ pcause' = c
         /\ UNCHANGED <<exitN, exitR>>
    ELSE UNCHANGED <<exitN, exitR, pst, pwhy, pcause>>

DeliverDown(c, r) ==
    /\ downN' = [downN EXCEPT ![c] = @ + 1]
    /\ downR' = [downR EXCEPT ![c] = r]

-----------------------------------------------------------------------------
(* Stage 1, in the caller. *)

Begin(c) ==
    /\ phase[c] = "idle"
    /\ \E k \in Kinds, e \in Explicit, n \in (IF Named THEN BOOLEAN ELSE {FALSE}):
         /\ kind' = [kind EXCEPT ![c] = k]
         /\ expl' = [expl EXCEPT ![c] = IF e = "none" THEN None ELSE e]
         /\ wname' = [wname EXCEPT ![c] = n]
    /\ phase' = [phase EXCEPT ![c] = "begun"]
    /\ UNCHANGED <<pV, place, admOpen, owned, readyOpen, eV, xV, cV, nV, mV, rV, dV>>

\* Preparation (executor resources, state) fails before anything is visible.
PrepareFail(c) ==
    /\ phase[c] = "begun"
    /\ phase' = [phase EXCEPT ![c] = "aborted"]
    /\ res' = [res EXCEPT ![c] = "system"]
    /\ UNCHANGED <<pV, oV, place, admOpen, owned, readyOpen, eV, xV,
                   commit, why, wait, pend, nV, mV, rV, dV>>

\* Why the commit refuses, or None. Order of the checks does not matter: all
\* are read under the commit locks.
Refusal(c) ==
    LET t == Target(c) IN
    CASE t = "G" /\ ~gopen /\ Fault # "AdmitClosed" -> "closed"
      [] t = "G" /\ LiveInG >= Cap /\ Fault # "IgnoreCap" -> "capacity"
      [] kind[c] \in {"link", "monitor"} /\ pst # "run" /\ Fault # "LinkCheckLate"
             -> "parentexited"
      [] wname[c] /\ reg # {} /\ Fault # "DoubleRegister" -> "already"
      [] OTHER -> None

\* The stage-1 commit: everything at once, or nothing.
Commit(c) ==
    /\ phase[c] = "begun"
    /\ Fault # "SplitCommit" \/ ~wname[c]
    /\ LET t == Target(c)
           r == Refusal(c)
           lk == kind[c] = "link" /\ Fault # "LinkAfterStarted"
           mn == kind[c] = "monitor"
       IN IF r # None
          THEN /\ phase' = [phase EXCEPT ![c] = "aborted"]
               /\ res' = [res EXCEPT ![c] = r]
               /\ UNCHANGED <<place, admOpen, owned, eV, wait, reg>>
          ELSE /\ phase' = [phase EXCEPT ![c] = "starting"]
               /\ place' = [place EXCEPT ![c] = t]
               /\ admOpen' = [admOpen EXCEPT ![c] = gopen]
               /\ owned' = [owned EXCEPT ![c] = kind[c] = "link" /\ pplace = "G" /\ t = "G"]
               /\ edge' = [edge EXCEPT ![c] = lk]
               /\ mon' = [mon EXCEPT ![c] = mn]
               /\ linkEver' = [linkEver EXCEPT ![c] = lk]
               /\ monEver' = [monEver EXCEPT ![c] = mn]
               /\ reg' = IF wname[c] THEN reg \cup {c} ELSE reg
               /\ wait' = [wait EXCEPT ![c] = IF Waiting(kind[c]) THEN "waiting" ELSE "none"]
               /\ res' = [res EXCEPT ![c] = IF Waiting(kind[c]) THEN "pending" ELSE "ok"]
    /\ UNCHANGED <<pV, oV, readyOpen, xV, commit, why, pend, nV, mV, seen, dV>>

\* SplitCommit (negative control): the name is published before the rest of
\* the commit, which may still refuse.
RegisterFirst(c) ==
    /\ Fault = "SplitCommit"
    /\ phase[c] = "begun" /\ wname[c] /\ reg = {}
    /\ reg' = reg \cup {c}
    /\ phase' = [phase EXCEPT ![c] = "reg1"]
    /\ UNCHANGED <<pV, oV, place, admOpen, owned, readyOpen, eV, xV, cV, nV, mV, seen, dV>>
CommitRest(c) ==
    /\ phase[c] = "reg1"
    /\ LET t == Target(c)
           r == CASE t = "G" /\ (~gopen \/ LiveInG >= Cap) -> "capacity"
                  [] kind[c] \in {"link", "monitor"} /\ pst # "run" -> "parentexited"
                  [] OTHER -> None
       IN IF r # None
          THEN /\ phase' = [phase EXCEPT ![c] = "aborted"]
               /\ res' = [res EXCEPT ![c] = r]
               /\ reg' = reg \ {c}
               /\ UNCHANGED <<place, admOpen, owned, eV, wait>>
          ELSE /\ phase' = [phase EXCEPT ![c] = "starting"]
               /\ place' = [place EXCEPT ![c] = t]
               /\ admOpen' = [admOpen EXCEPT ![c] = gopen]
               /\ owned' = [owned EXCEPT ![c] = kind[c] = "link" /\ pplace = "G" /\ t = "G"]
               /\ edge' = [edge EXCEPT ![c] = kind[c] = "link"]
               /\ mon' = [mon EXCEPT ![c] = kind[c] = "monitor"]
               /\ linkEver' = [linkEver EXCEPT ![c] = kind[c] = "link"]
               /\ monEver' = [monEver EXCEPT ![c] = kind[c] = "monitor"]
               /\ wait' = [wait EXCEPT ![c] = IF Waiting(kind[c]) THEN "waiting" ELSE "none"]
               /\ res' = [res EXCEPT ![c] = IF Waiting(kind[c]) THEN "pending" ELSE "ok"]
               /\ UNCHANGED reg
    /\ UNCHANGED <<pV, oV, readyOpen, xV, commit, why, pend, nV, mV, seen, dV>>

\* Anyone resolves the name (whereis).
Lookup ==
    /\ \E c \in reg :
         /\ ~seen[c]
         /\ seen' = [seen EXCEPT ![c] = TRUE]
    /\ UNCHANGED <<pV, oV, sV, eV, xV, cV, nV, mV, reg, dV>>

-----------------------------------------------------------------------------
(* Stage 2, on the child. *)

Exit(c, r) ==
    /\ phase' = [phase EXCEPT ![c] = "exiting"]
    /\ why' = [why EXCEPT ![c] = r]

StartedBegin(c) ==
    /\ phase[c] = "starting" /\ ~began[c] /\ ~killed[c] /\ ~skill[c]
    /\ began' = [began EXCEPT ![c] = TRUE]
    /\ UNCHANGED <<pV, oV, sV, eV, killed, skill, pex, qpex, pexAbn, ctrap,
                   cV, nV, mV, rV, dV>>

\* LinkAfterStarted (negative control): the link is set once started() runs.
LateLink(c) ==
    /\ Fault = "LinkAfterStarted"
    /\ kind[c] = "link" /\ began[c] /\ ~linkEver[c]
    /\ phase[c] \in {"starting", "ready"} /\ pst = "run"
    /\ edge' = [edge EXCEPT ![c] = TRUE]
    /\ linkEver' = [linkEver EXCEPT ![c] = TRUE]
    /\ UNCHANGED <<pV, oV, sV, mon, monEver, xV, cV, nV, mV, rV, dV>>

\* A kill (init timer or scope deadline) before started() began.
KilledEarly(c) ==
    /\ phase[c] = "starting" /\ ~began[c] /\ (killed[c] \/ skill[c])
    /\ Exit(c, "Killed")
    /\ commit' = IF commit[c] = None THEN [commit EXCEPT ![c] = "failed"] ELSE commit
    /\ UNCHANGED <<pV, oV, place, admOpen, owned, readyOpen, eV, xV,
                   wait, res, pend, nV, mV, rV, dV>>

\* started() returns (or panics); held signals are resolved now, with the
\* trap flag it leaves (design section 5.3, precedence):
\*   1. the init timer committed timedout, or a kill: exit Killed;
\*   2. a non-Normal held parent exit and the child does not trap: exit
\*      with it (a held Normal parent exit is dropped);
\*   3. a panic, Init::stop (non-Normal), Init::error / ignore (Normal);
\*   4. ready: commit, unless the group closed (exit Shutdown). A held parent
\*      exit to a trapping child is queued behind the continuation (E3).
StartedEnd(c) ==
    /\ phase[c] = "starting" /\ began[c]
    /\ \E o \in Outcomes, t \in (IF ChildCanTrap THEN BOOLEAN ELSE {FALSE}):
       LET sigOut == pex[c] \in NonNormal /\ ~t /\ Fault # "DropHeld"
           failWith(r) == /\ Exit(c, r)
                          /\ commit' = IF commit[c] = None
                                       THEN [commit EXCEPT ![c] = "failed"] ELSE commit
                          /\ UNCHANGED <<cont, readyOpen, qpex>>
       IN /\ ctrap' = [ctrap EXCEPT ![c] = t]
          /\ pex' = [pex EXCEPT ![c] = None]
          /\ IF killed[c] \/ skill[c] THEN failWith("Killed")
             ELSE IF sigOut THEN failWith(pex[c])
             ELSE IF o = "panic" THEN failWith("Panic")
             ELSE IF o = "stop" THEN failWith("Shutdown")
             ELSE IF o \in {"error", "ignore"}
             THEN /\ Exit(c, "Normal")
                  /\ commit' = [commit EXCEPT ![c] = IF o = "ignore" THEN "ignored" ELSE "failed"]
                  /\ UNCHANGED <<cont, readyOpen, qpex>>
             ELSE IF place[c] = "G" /\ ~gopen /\ Fault # "ReadyAfterClose"
             THEN failWith("Shutdown")
             ELSE /\ phase' = [phase EXCEPT ![c] = "ready"]
                  /\ commit' = [commit EXCEPT ![c] = "ready"]
                  /\ readyOpen' = [readyOpen EXCEPT ![c] = gopen \/ place[c] # "G"]
                  /\ cont' = [cont EXCEPT ![c] = o = "continue"]
                  /\ qpex' = [qpex EXCEPT ![c] = IF t THEN pex[c] ELSE None]
                  /\ UNCHANGED why
    /\ UNCHANGED <<pV, oV, place, admOpen, owned, eV, began, killed, skill,
                   pexAbn, wait, res, pend, nV, msgs, early, rV, dV>>

\* Others (a peer that found the name, the caller after the ack) send.
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

\* A queued parent exit to a trapping child ends it, Normal included (E3);
\* it is a mailbox entry, so it waits for the continuation.
HandleQueuedExit(c) ==
    /\ phase[c] = "ready" /\ qpex[c] # None
    /\ ~cont[c] \/ Fault = "ContinueQueued"
    /\ early' = [early EXCEPT ![c] = early[c] \/ cont[c]]
    /\ qpex' = [qpex EXCEPT ![c] = None]
    /\ Exit(c, qpex[c])
    /\ UNCHANGED <<pV, oV, place, admOpen, owned, readyOpen, eV, began, killed,
                   skill, pex, pexAbn, ctrap, commit, wait, res, pend, nV,
                   cont, msgs, rV, dV>>

\* A parent exit reaching a ready, non-trapping child (Normal is ignored).
ApplyParentExit(c) ==
    /\ phase[c] = "ready" /\ pex[c] # None
    /\ pex' = [pex EXCEPT ![c] = None]
    /\ IF pex[c] \in NonNormal THEN Exit(c, pex[c]) ELSE UNCHANGED <<phase, why>>
    /\ UNCHANGED <<pV, oV, place, admOpen, owned, readyOpen, eV, began, killed,
                   skill, qpex, pexAbn, ctrap, commit, wait, res, pend, nV, mV, rV, dV>>

\* After readiness the child may stop or crash on its own.
ReadyExit(c) ==
    /\ phase[c] = "ready"
    /\ \E r \in {"Normal", "Panic"} : Exit(c, r)
    /\ UNCHANGED <<pV, oV, place, admOpen, owned, readyOpen, eV, xV,
                   commit, wait, res, pend, nV, mV, rV, dV>>

\* A kill reaching a ready child (the scope deadline; under
\* TimerIgnoresCommit also the init timer).
KillReady(c) ==
    /\ phase[c] = "ready" /\ (killed[c] \/ skill[c])
    /\ Exit(c, "Killed")
    /\ UNCHANGED <<pV, oV, place, admOpen, owned, readyOpen, eV, xV,
                   commit, wait, res, pend, nV, mV, rV, dV>>

\* A ready linked child unlinks from P (ctx.unlink); ownership stays (S8).
ChildUnlink(c) ==
    /\ UnlinkEnabled /\ phase[c] = "ready" /\ edge[c]
    /\ edge' = [edge EXCEPT ![c] = FALSE]
    /\ UNCHANGED <<pV, oV, sV, mon, linkEver, monEver, xV, cV, nV, mV, rV, dV>>

\* D: the edges close and their notices are taken (delivered at publication).
Die(c) ==
    /\ phase[c] = "exiting"
    /\ phase' = [phase EXCEPT ![c] = "dead"]
    /\ lnote' = [lnote EXCEPT ![c] = edge[c] /\ pst # "dead"]
    /\ mnote' = [mnote EXCEPT ![c] = mon[c] /\ pst # "dead"]
    /\ edge' = [edge EXCEPT ![c] = FALSE]
    /\ mon' = [mon EXCEPT ![c] = FALSE]
    /\ reg' = IF Fault = "UnregisterAtDeath" THEN reg \ {c} ELSE reg
    /\ UNCHANGED <<pV, oV, place, admOpen, owned, readyOpen, linkEver, monEver,
                   xV, cV, div, divEver, mV, seen, dV>>

\* Publication (after destruction): the name and the slot are released,
\* then each taken notice is disposed of by the startup cell: a failed start
\* whose waiter still waits (or consumed the error) diverts or drops it,
\* except that a non-trapping P still exits on a non-Normal init failure
\* (OTP: the link kills the caller); otherwise it is an ordinary notice.
Publish(c) ==
    /\ phase[c] = "dead"
    /\ phase' = [phase EXCEPT ![c] = "published"]
    /\ reg' = IF Fault = "LateUnregister" THEN reg ELSE reg \ {c}
    /\ LET r == why[c]
           failedStart == (commit[c] \in {"failed", "ignored", "timedout"}
                           \/ (Fault = "DivertAfterReady" /\ commit[c] = "ready"))
                          /\ wait[c] \in {"waiting", "consumed"}
           keepTimeout == commit[c] = "timedout" /\ Fault = "KillBeforeUnlink"
           ordinary == ~failedStart \/ keepTimeout
           ln == lnote[c] /\ pst # "dead"
           mn == mnote[c] /\ pst # "dead"
           divert == failedStart /\ wait[c] = "waiting"
                     /\ ((ln /\ ptrap) \/ mn) /\ ~keepTimeout
       IN /\ div' = IF divert THEN [div EXCEPT ![c] = r] ELSE div
          /\ divEver' = IF divert THEN [divEver EXCEPT ![c] = TRUE] ELSE divEver
          /\ IF ln /\ (ordinary \/ (ptrap /\ Fault = "NoLinkFlush")
                       \/ (ptrap /\ divert /\ Fault = "DoubleNotice"))
             THEN /\ DeliverLink(c, r) /\ UNCHANGED <<downN, downR>>
             ELSE IF ln /\ ~ptrap /\ commit[c] = "failed" /\ r # "Normal" /\ pst = "run"
             THEN /\ pst' = "exiting" /\ pwhy' = r /\ pcause' = c
                  /\ UNCHANGED dV
             ELSE IF mn /\ (ordinary \/ Fault = "NoMonitorFlush")
             THEN /\ DeliverDown(c, r) /\ UNCHANGED <<exitN, exitR, pst, pwhy, pcause>>
             ELSE UNCHANGED <<dV, pst, pwhy, pcause>>
    /\ UNCHANGED <<pplace, ptrap, gopen, gdone, oV, place, admOpen, owned, readyOpen,
                   eV, xV, cV, lnote, mnote, mV, seen>>

\* LateUnregister (negative control): the name is released only after the
\* notices.
LateRelease(c) ==
    /\ Fault = "LateUnregister" /\ phase[c] = "published" /\ c \in reg
    /\ reg' = reg \ {c}
    /\ UNCHANGED <<pV, oV, sV, eV, xV, cV, nV, mV, seen, dV>>

-----------------------------------------------------------------------------
(* The init timer and the waiter. *)

\* The framework-owned init timer: it commits timedout only if nothing was
\* committed, then kills. The start edges stay; their notice is disposed of
\* at publication by the waiter state (contained while waiting or consumed).
Timeout(c) ==
    /\ TimeoutEnabled
    /\ Waiting(kind[c]) \/ kind[c] = "spawn"
    /\ ~killed[c]
    /\ \/ commit[c] = None /\ phase[c] = "starting"
       \/ Fault = "TimerIgnoresCommit" /\ phase[c] \in {"starting", "ready"}
    /\ commit' = IF commit[c] = None THEN [commit EXCEPT ![c] = "timedout"] ELSE commit
    /\ killed' = [killed EXCEPT ![c] = TRUE]
    /\ UNCHANGED <<pV, oV, sV, eV, began, skill, pex, qpex, pexAbn, ctrap,
                   why, wait, res, pend, nV, mV, rV, dV>>

AckOk(c) ==
    /\ wait[c] = "waiting"
    /\ \/ commit[c] = "ready"
       \/ Fault = "AckBeforeStarted" /\ commit[c] = None /\ phase[c] = "starting"
       \/ Fault = "ErrorAsOk" /\ commit[c] = "ignored"
    /\ res' = [res EXCEPT ![c] = "ok"]
    /\ wait' = [wait EXCEPT ![c] = "consumed"]
    /\ UNCHANGED <<pV, oV, sV, eV, xV, commit, why, pend, nV, mV, rV, dV>>

\* A failed, ignored or timed-out start answers after publication, or at its
\* bounded cleanup deadline with the cleanup still pending (pend). The error
\* consumes a diverted notice.
AckFail(c) ==
    /\ wait[c] = "waiting"
    /\ \/ commit[c] \in {"failed", "ignored", "timedout"}
       \/ Fault = "DivertAfterReady" /\ commit[c] = "ready" /\ phase[c] = "published"
    /\ \E late \in BOOLEAN :
         /\ IF late THEN phase[c] \in {"exiting", "dead"}
                           \/ (phase[c] = "starting" /\ commit[c] = "timedout")
            ELSE \/ phase[c] = "published"
                 \/ Fault = "EarlyFailAck" /\ phase[c] = "dead"
         /\ pend' = [pend EXCEPT ![c] = late]
    /\ ~(Fault = "NoAnswerIgnore" /\ commit[c] = "ignored")
    /\ res' = [res EXCEPT ![c] = CASE commit[c] = "ignored" -> "ignored"
                                   [] commit[c] = "timedout" -> "timeout"
                                   [] OTHER -> "failed"]
    /\ wait' = [wait EXCEPT ![c] = "consumed"]
    /\ div' = [div EXCEPT ![c] = None]
    /\ UNCHANGED <<pV, oV, sV, eV, xV, commit, why, lnote, mnote, divEver, mV, rV, dV>>

\* The Starting is dropped: the child keeps running; a diverted notice goes
\* to P as an ordinary one; later notices are ordinary.
Abandon(c) ==
    /\ wait[c] = "waiting"
    /\ wait' = [wait EXCEPT ![c] = "abandoned"]
    /\ div' = [div EXCEPT ![c] = None]
    /\ LET r == div[c]
           give == r # None /\ Fault # "AbandonDropsNotice"
       IN IF give /\ kind[c] = "link" THEN DeliverLink(c, r) /\ UNCHANGED <<downN, downR>>
          ELSE IF give /\ kind[c] = "monitor"
          THEN DeliverDown(c, r) /\ UNCHANGED <<exitN, exitR, pst, pwhy, pcause>>
          ELSE UNCHANGED <<dV, pst, pwhy, pcause>>
    /\ UNCHANGED <<pplace, ptrap, gopen, gdone, oV, sV, eV, xV, commit, why,
                   res, pend, lnote, mnote, divEver, mV, rV>>

-----------------------------------------------------------------------------
(* The parent, its scope and the group. *)

PExit ==
    /\ ParentCanExit /\ pst = "run"
    /\ \E r \in {"Normal", "Panic"} : pst' = "exiting" /\ pwhy' = r
    /\ UNCHANGED <<pplace, ptrap, pcause, gopen, gdone, oV, sV, eV, xV, cV, nV, mV, rV, dV>>

\* P's D: each linked child gets a parent exit, held during started(),
\* queued behind the continuation for a trapping ready child, applied
\* otherwise; P's monitors are cancelled.
PDie ==
    /\ pst = "exiting"
    /\ pst' = "dead"
    /\ pex' = [c \in Kids |-> IF edge[c] /\ ~(phase[c] = "ready" /\ ctrap[c])
                              THEN pwhy ELSE pex[c]]
    /\ qpex' = [c \in Kids |-> IF edge[c] /\ phase[c] = "ready" /\ ctrap[c]
                               THEN pwhy ELSE qpex[c]]
    /\ pexAbn' = [c \in Kids |-> pexAbn[c] \/ (edge[c] /\ pwhy \in NonNormal)]
    /\ edge' = [c \in Kids |-> FALSE]
    /\ mon' = [c \in Kids |-> FALSE]
    /\ UNCHANGED <<pplace, ptrap, pwhy, pcause, gopen, gdone, oV, sV,
                   linkEver, monEver, began, killed, skill, ctrap, cV, nV, mV, rV, dV>>

\* S8: once P's exit began, the scope deadline kills every owned child
\* still alive (linked or not).
ScopeDeadline ==
    /\ ScopeEnabled /\ pplace = "G" /\ pst # "run"
    /\ \E c \in Kids : owned[c] /\ phase[c] \in {"starting", "ready"} /\ ~skill[c]
                       /\ (edge[c] \/ Fault # "NoScopeKill")
    /\ skill' = [c \in Kids |-> skill[c] \/ (owned[c] /\ phase[c] \in {"starting", "ready"}
                                             /\ (edge[c] \/ Fault # "NoScopeKill"))]
    /\ UNCHANGED <<pV, oV, sV, eV, began, killed, pex, qpex, pexAbn, ctrap, cV, nV, mV, rV, dV>>

\* P's generation is done: P past D and every owned child published.
GenDone ==
    /\ ScopeEnabled /\ pplace = "G" /\ pst = "dead" /\ ~gdone
    /\ \A c \in Kids : owned[c] => phase[c] \in {"published", "aborted"}
                                   \/ Fault = "EarlyGenDone"
    /\ gdone' = TRUE
    /\ UNCHANGED <<pst, pplace, ptrap, pwhy, pcause, gopen, oV, sV, eV, xV, cV, nV, mV, rV, dV>>

GClose ==
    /\ CloseEnabled /\ gopen
    /\ gopen' = FALSE
    /\ UNCHANGED <<pst, pplace, ptrap, pwhy, pcause, gdone, oV, sV, eV, xV, cV, nV, mV, rV, dV>>

ChildStep(c) ==
    \/ Begin(c) \/ PrepareFail(c) \/ Commit(c) \/ RegisterFirst(c) \/ CommitRest(c)
    \/ StartedBegin(c) \/ LateLink(c) \/ KilledEarly(c) \/ StartedEnd(c)
    \/ Send(c) \/ RunContinue(c) \/ Handle(c) \/ HandleQueuedExit(c)
    \/ ApplyParentExit(c) \/ ReadyExit(c) \/ KillReady(c) \/ ChildUnlink(c)
    \/ Die(c) \/ Publish(c) \/ LateRelease(c)
    \/ Timeout(c) \/ AckOk(c) \/ AckFail(c) \/ Abandon(c)

Next == (\E c \in Kids : ChildStep(c)) \/ Lookup \/ PExit \/ PDie
        \/ ScopeDeadline \/ GenDone \/ GClose

\* Liveness assumes the child's own steps, publication, the waiter's answer,
\* P's death and the scope deadline complete (started() and destructors
\* return).
Progress(c) ==
    \/ StartedBegin(c) \/ KilledEarly(c) \/ StartedEnd(c) \/ RunContinue(c)
    \/ HandleQueuedExit(c) \/ ApplyParentExit(c) \/ KillReady(c)
    \/ Die(c) \/ Publish(c) \/ AckOk(c)
Fairness ==
    /\ \A c \in Kids : /\ WF_vars(Progress(c)) /\ WF_vars(Commit(c))
                       /\ SF_vars(AckFail(c))
    /\ WF_vars(PDie) /\ WF_vars(ScopeDeadline) /\ WF_vars(GenDone)

Spec == Init /\ [][Next]_vars
LiveSpec == Spec /\ Fairness

-----------------------------------------------------------------------------
(* Safety. *)

InheritedPlacement == \A c \in Kids : place[c] # None => place[c] = Expected(c)

\* The edge really exists before started() begins.
LinkBeforeStarted ==
    \A c \in Kids :
        /\ (began[c] /\ kind[c] = "link") => linkEver[c]
        /\ (began[c] /\ kind[c] = "monitor") => monEver[c]

NameUnique == Cardinality(reg) <= 1

\* The name is held from the commit until publication.
NameHeldUntilPublished == \A c \in reg : phase[c] \in {"reg1", "starting", "ready", "exiting", "dead"}

\* A new holder never overlaps an old holder's destruction.
NoOverlapOnReuse ==
    \A c \in reg : \A d \in Kids \ {c} : wname[d] => phase[d] # "dead"

\* No notice or final error about c is observable while c holds the name.
NameFreeBeforeObservation ==
    \A c \in Kids :
        (exitN[c] + downN[c] > 0 \/ (res[c] \in {"failed", "ignored", "timeout"} /\ ~pend[c]))
            => c \notin reg

\* A refused start is never observable.
AbortInvisible ==
    \A c \in Kids : phase[c] = "aborted" =>
        /\ ~edge[c] /\ ~mon[c] /\ ~linkEver[c] /\ ~monEver[c] /\ c \notin reg
        /\ ~seen[c] /\ ~began[c] /\ place[c] = None
        /\ exitN[c] = 0 /\ downN[c] = 0 /\ pcause # c

\* The answer is the committed outcome.
ResultMatchesCommit ==
    \A c \in Kids :
        /\ res[c] = "ok" /\ Waiting(kind[c]) => commit[c] = "ready"
        /\ res[c] = "failed" => commit[c] = "failed"
        /\ res[c] = "ignored" => commit[c] = "ignored"
        /\ res[c] = "timeout" => commit[c] = "timedout"

\* Readiness is final: a ready child's later death is never a start error.
ReadyIsFinal == \A c \in Kids : commit[c] = "ready" => res[c] \notin {"failed", "ignored", "timeout"}

\* The init timer never kills a child that committed ready.
TimeoutNeverKillsReady == \A c \in Kids : commit[c] = "ready" => ~killed[c]

\* Managed readiness commits only while admission is open.
ReadyOnlyWhileOpen == \A c \in Kids : commit[c] = "ready" => readyOpen[c]

\* Without a pending cleanup, a start error is answered after publication.
FailureAfterPublication ==
    \A c \in Kids : (res[c] \in {"failed", "ignored", "timeout"} /\ ~pend[c]) => phase[c] = "published"

\* A failed start_monitor delivers no Down; a failed start_link no Exit to a
\* trapping parent.
FailureFlushed ==
    \A c \in Kids : res[c] \in {"failed", "ignored", "timeout"} =>
        /\ kind[c] = "monitor" => downN[c] = 0
        /\ (kind[c] = "link" /\ ptrap) => exitN[c] = 0

\* An init timeout reaches P only if the wait was abandoned.
TimeoutContained ==
    \A c \in Kids : (commit[c] = "timedout" /\ wait[c] # "abandoned") =>
        /\ pcause # c /\ exitN[c] = 0 /\ downN[c] = 0

NoEdgeToDeadParent == \A c \in Kids : (edge[c] \/ mon[c]) => pst # "dead"

AdmittedOnlyOpen == \A c \in Kids : place[c] = "G" => admOpen[c]

CapacityRespected == LiveInG <= Cap

NoticeOnce == \A c \in Kids : exitN[c] <= 1 /\ downN[c] <= 1

\* A notice diverted to an abandoned wait reaches P, with the child's reason.
AbandonedNoticeDelivered ==
    \A c \in Kids : (wait[c] = "abandoned" /\ divEver[c]) =>
        /\ kind[c] = "link" => exitN[c] >= 1 /\ exitR[c] = why[c]
        /\ kind[c] = "monitor" => downN[c] >= 1 /\ downR[c] = why[c]

ContinueFirst == \A c \in Kids : ~early[c]

\* P's generation is done only after its owned children.
GenDoneMeansOwnedDone ==
    gdone => \A c \in Kids : owned[c] => phase[c] \in {"published", "aborted"}

Safety ==
    /\ InheritedPlacement /\ LinkBeforeStarted /\ NameUnique
    /\ NameHeldUntilPublished /\ NoOverlapOnReuse /\ NameFreeBeforeObservation
    /\ AbortInvisible /\ ResultMatchesCommit /\ ReadyIsFinal
    /\ TimeoutNeverKillsReady /\ ReadyOnlyWhileOpen /\ FailureAfterPublication
    /\ FailureFlushed /\ TimeoutContained /\ NoEdgeToDeadParent
    /\ AdmittedOnlyOpen /\ CapacityRespected /\ NoticeOnce
    /\ AbandonedNoticeDelivered /\ ContinueFirst /\ GenDoneMeansOwnedDone

(* Liveness. *)
\* A child linked to a parent that dies with a non-Normal reason ends.
ParentExitReachesChild ==
    \A c \in Kids : pexAbn[c] ~> phase[c] \in {"published", "aborted"}
\* Every waiting start is answered or abandoned.
StartAnswered == \A c \in Kids : wait[c] = "waiting" ~> wait[c] # "waiting"
\* A managed parent's generation finishes once it died.
OwnedScopeFinishes == (ScopeEnabled /\ pplace = "G" /\ pst = "dead") ~> gdone
=============================================================================
