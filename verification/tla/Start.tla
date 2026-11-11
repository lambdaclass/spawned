------------------------------- MODULE Start -------------------------------
EXTENDS Naturals, FiniteSets
(***************************************************************************)
(* Unified start API (0.9; design: docs/design/unified-start.md).          *)
(*                                                                         *)
(* One parent actor P starts children with spawn, start, start_link or     *)
(* start_monitor, each with optional StartOptions (explicit placement,     *)
(* a registered name). A start has two stages:                             *)
(*   stage 1 (in the caller, before the child runs any code):              *)
(*     Admit    placement: explicit, else inherited (link/monitor), else   *)
(*              unmanaged; a group admits only while open and below Cap;   *)
(*     Link     start_link/start_monitor: the edge is recorded while P is  *)
(*              running, else the start is refused (NoProc-like);          *)
(*     Register the name, atomically, or AlreadyStarted;                   *)
(*     Dispatch the child: spawn returns here (no ack);                    *)
(*   stage 2 (on the child): started() returns Init (ready, or ready with  *)
(*     a continuation), returns an error or ignore (exit Normal), or       *)
(*     panics or stops abnormally; signals received meanwhile are held     *)
(*     until it returns. A continuation runs before any mailbox message.   *)
(*     start, start_link and start_monitor return after started() (the     *)
(*     ack): Ok when it returned ready with no exit pending, else the     *)
(*     error after *)
(*     the child's death point D, consuming the link notice (trapping      *)
(*     parent) or the Down. A startup timeout unlinks and demonitors       *)
(*     first, then kills.                                                  *)
(* An aborted stage 1 leaves no trace: no edge, no name, no slot, no       *)
(* hook, no notice. Children never trap exits; a parent exit is held       *)
(* during started() and applied after it (Normal is ignored).              *)
(*                                                                         *)
(* Steps are atomic; interleavings between steps of stage 1 are what the   *)
(* implementation must make invisible (parent death, group closure and a  *)
(* competing registration may happen between any two of them).            *)
(***************************************************************************)
CONSTANTS
    Kids,           \* child names
    Kinds,          \* variants used: subset of {"spawn","start","link","monitor"}
    Explicit,       \* explicit placements: subset of {"none","G","U","same"}
                    \* ("same" = StartOptions::placement_of(&ctx))
    ParentPlaces,   \* where P runs: subset of {"G","U"}
    Cap,            \* live actors group G admits (thread_group workers)
    Named,          \* children may request the one shared name
    TimeoutEnabled, \* start_link/start_monitor may time out
    ParentCanExit,  \* P may exit (with Normal or Panic)
    CloseEnabled,   \* G may close (system shutdown)
    MaxMsgs,        \* messages others may send to each child
    Fault           \* "None" or a negative control, see below

(* Negative controls (each must produce a counterexample):                 *)
(*  NoInherit        0.8: a linked/monitored child of a managed parent is  *)
(*                   unmanaged                    -> InheritedPlacement    *)
(*  LinkAfterStarted the link is set after started() began                 *)
(*                                                -> LinkBeforeStarted     *)
(*  DoubleRegister   check-then-insert name registration -> NameUnique     *)
(*  LateUnregister   the name outlives D          -> NameReleasedAtDeath   *)
(*  AbortLeaksLink   a refused start keeps its edge -> AbortInvisible      *)
(*  EarlyFailAck     the error returns before the child's D                *)
(*                                                -> FailureAfterDeath     *)
(*  AckBeforeStarted Ok before started() returned -> AckMeansStarted       *)
(*  NoMonitorFlush   a failed start_monitor also delivers Down             *)
(*                                                -> FailureFlushed        *)
(*  NoLinkFlush      a failed start_link also delivers Exit to a trapping  *)
(*                   parent                       -> FailureFlushed        *)
(*  KillBeforeUnlink the timeout kills while still linked                  *)
(*                                                -> TimeoutContained      *)
(*  LinkCheckLate    the link ignores the parent's state                   *)
(*                                                -> NoEdgeToDeadParent    *)
(*  AdmitClosed      a closed group admits        -> AdmittedOnlyOpen      *)
(*  IgnoreCap        a full group admits          -> CapacityRespected     *)
(*  ContinueQueued   the continuation is queued behind earlier messages    *)
(*                                                -> ContinueFirst         *)
(*  AbandonDropsNotice a dropped start future loses the diverted notice    *)
(*                                     -> AbandonedNoticeDelivered         *)
(*  DropHeld         a parent exit held during started() is dropped        *)
(*                                (liveness)      -> ParentExitReachesChild*)

None == "none"
Abnormal == {"Panic", "Killed", "NoProc", "Shutdown"}
Reasons == {"Normal"} \cup Abnormal
Phases == {"idle", "begun", "admitted", "linked", "registered", "starting",
           "ready", "exiting", "dead", "aborted"}
Results == {"none", "pending", "ok", "failed", "timeout", "closed",
            "capacity", "already", "noproc", "abandoned"}

VARIABLES
    pst,      \* P: "run", "exiting" (exit begun, before D) or "dead" (past D)
    pplace,   \* P's placement: "G" (managed) or "U" (unmanaged)
    ptrap,    \* P traps exits
    pwhy,     \* P's exit reason
    pcause,   \* the child whose death notice made P exit, or None
    gopen,    \* G admits
    kind,     \* [Kids -> start variant]
    expl,     \* [Kids -> explicit placement option]
    wname,    \* [Kids -> requests the name]
    phase,    \* [Kids -> Phases]
    place,    \* [Kids -> "G", "U" or None]
    admOpen,  \* [Kids -> G was open when the child was admitted to it]
    edge,     \* [Kids -> link edge with P]
    mon,      \* [Kids -> P's monitor on the child]
    linkedAt, \* [Kids -> None, "before" or "after" started() began]
    monAt,    \* same for the monitor
    began,    \* [Kids -> started() began]
    retd,     \* [Kids -> started() returned, panicked or stopped]
    killed,   \* [Kids -> startup-timeout kill received]
    pex,      \* [Kids -> parent exit queued at the child, or None]
    pexAbn,   \* [Kids -> a non-Normal parent exit was ever queued]
    cont,     \* [Kids -> a continuation is pending]
    msgs,     \* [Kids -> messages sent to the child by others]
    early,    \* [Kids -> a message was handled before the continuation]
    div,      \* [Kids -> notice diverted to the waiting start call, or None]
    divEver,  \* [Kids -> a notice was ever diverted]
    why,      \* [Kids -> exit reason or None]
    res,      \* [Kids -> what the start call returned]
    reg,      \* children holding the name
    exits,    \* Exit messages P's exit_received sees: [c, r]
    downs     \* Down messages P sees: [c, r]

vars == <<pst, pplace, ptrap, pwhy, pcause, gopen, kind, expl, wname, phase,
          place, admOpen, edge, mon, linkedAt, monAt, began, retd, killed,
          pex, pexAbn, cont, msgs, early, div, divEver, why, res, reg, exits, downs>>
pVars == <<pst, pplace, ptrap, pwhy, pcause, gopen>>
optVars == <<kind, expl, wname>>
startVars == <<began, retd, killed, pex, pexAbn, why, cont, msgs, early>>
obsVars == <<reg, exits, downs, div, divEver>>

Live(c) == phase[c] \in {"admitted", "linked", "registered", "starting",
                         "ready", "exiting"}
LiveInG == Cardinality({c \in Kids : place[c] = "G" /\ Live(c)})
           + (IF pplace = "G" /\ pst # "dead" THEN 1 ELSE 0)

\* The contract: explicit placement, else the parent's (link/monitor),
\* else unmanaged.
Expected(c) ==
    CASE expl[c] = "same" -> pplace
      [] expl[c] \in {"G", "U"} -> expl[c]
      [] kind[c] \in {"link", "monitor"} -> pplace
      [] OTHER -> "U"
Target(c) ==
    IF expl[c] = None /\ kind[c] \in {"link", "monitor"} /\ Fault = "NoInherit"
    THEN "U" ELSE Expected(c)

TypeOK ==
    /\ pst \in {"run", "exiting", "dead"}
    /\ pplace \in {"G", "U"}
    /\ ptrap \in BOOLEAN
    /\ pwhy \in Reasons \cup {None}
    /\ pcause \in Kids \cup {None}
    /\ gopen \in BOOLEAN
    /\ kind \in [Kids -> {"spawn", "start", "link", "monitor", None}]
    /\ expl \in [Kids -> {"G", "U", "same", None}]
    /\ wname \in [Kids -> BOOLEAN]
    /\ phase \in [Kids -> Phases]
    /\ place \in [Kids -> {"G", "U", None}]
    /\ admOpen \in [Kids -> BOOLEAN]
    /\ edge \in [Kids -> BOOLEAN]
    /\ mon \in [Kids -> BOOLEAN]
    /\ linkedAt \in [Kids -> {None, "before", "after"}]
    /\ monAt \in [Kids -> {None, "before", "after"}]
    /\ began \in [Kids -> BOOLEAN]
    /\ retd \in [Kids -> BOOLEAN]
    /\ killed \in [Kids -> BOOLEAN]
    /\ pex \in [Kids -> Reasons \cup {None}]
    /\ pexAbn \in [Kids -> BOOLEAN]
    /\ cont \in [Kids -> BOOLEAN]
    /\ msgs \in [Kids -> 0..MaxMsgs]
    /\ early \in [Kids -> BOOLEAN]
    /\ why \in [Kids -> Reasons \cup {None}]
    /\ res \in [Kids -> Results]
    /\ reg \subseteq Kids
    /\ exits \subseteq [c : Kids, r : Reasons]
    /\ downs \subseteq [c : Kids, r : Reasons]
    /\ div \in [Kids -> Reasons \cup {None}]
    /\ divEver \in [Kids -> BOOLEAN]

Init ==
    /\ pst = "run"
    /\ pplace \in ParentPlaces
    /\ ptrap \in BOOLEAN
    /\ pwhy = None
    /\ pcause = None
    /\ gopen = TRUE
    /\ kind = [c \in Kids |-> None]
    /\ expl = [c \in Kids |-> None]
    /\ wname = [c \in Kids |-> FALSE]
    /\ phase = [c \in Kids |-> "idle"]
    /\ place = [c \in Kids |-> None]
    /\ admOpen = [c \in Kids |-> FALSE]
    /\ edge = [c \in Kids |-> FALSE]
    /\ mon = [c \in Kids |-> FALSE]
    /\ linkedAt = [c \in Kids |-> None]
    /\ monAt = [c \in Kids |-> None]
    /\ began = [c \in Kids |-> FALSE]
    /\ retd = [c \in Kids |-> FALSE]
    /\ killed = [c \in Kids |-> FALSE]
    /\ pex = [c \in Kids |-> None]
    /\ pexAbn = [c \in Kids |-> FALSE]
    /\ cont = [c \in Kids |-> FALSE]
    /\ msgs = [c \in Kids |-> 0]
    /\ early = [c \in Kids |-> FALSE]
    /\ why = [c \in Kids |-> None]
    /\ res = [c \in Kids |-> "none"]
    /\ reg = {}
    /\ exits = {}
    /\ downs = {}
    /\ div = [c \in Kids |-> None]
    /\ divEver = [c \in Kids |-> FALSE]

-----------------------------------------------------------------------------
(* Stage 1, in the caller. *)

\* The caller asks for a start: variant, explicit placement, name.
Begin(c) ==
    /\ phase[c] = "idle"
    /\ \E k \in Kinds, e \in Explicit, n \in (IF Named THEN BOOLEAN ELSE {FALSE}):
         /\ kind' = [kind EXCEPT ![c] = k]
         /\ expl' = [expl EXCEPT ![c] = IF e = "none" THEN None ELSE e]
         /\ wname' = [wname EXCEPT ![c] = n]
    /\ phase' = [phase EXCEPT ![c] = "begun"]
    /\ UNCHANGED <<pVars, place, admOpen, edge, mon, linkedAt, monAt,
                   startVars, res, obsVars>>

\* A refused start: nothing remains (edge, monitor, name and slot released;
\* no hook ran, no notice is sent).
Abort(c, r) ==
    /\ phase' = [phase EXCEPT ![c] = "aborted"]
    /\ res' = [res EXCEPT ![c] = r]
    /\ place' = [place EXCEPT ![c] = None]
    /\ edge' = IF Fault = "AbortLeaksLink" THEN edge ELSE [edge EXCEPT ![c] = FALSE]
    /\ mon' = [mon EXCEPT ![c] = FALSE]
    /\ reg' = reg \ {c}

Admit(c) ==
    /\ phase[c] = "begun"
    /\ LET t == Target(c) IN
         IF t = "U"
         THEN /\ phase' = [phase EXCEPT ![c] = "admitted"]
              /\ place' = [place EXCEPT ![c] = "U"]
              /\ UNCHANGED <<admOpen, res, edge, mon, reg>>
         ELSE IF (gopen \/ Fault = "AdmitClosed") /\ (LiveInG < Cap \/ Fault = "IgnoreCap")
         THEN /\ phase' = [phase EXCEPT ![c] = "admitted"]
              /\ place' = [place EXCEPT ![c] = "G"]
              /\ admOpen' = [admOpen EXCEPT ![c] = gopen]
              /\ UNCHANGED <<res, edge, mon, reg>>
         ELSE /\ Abort(c, IF ~gopen THEN "closed" ELSE "capacity")
              /\ UNCHANGED admOpen
    /\ UNCHANGED <<pVars, optVars, linkedAt, monAt, startVars, exits, downs, div, divEver>>

\* start_link / start_monitor: the edge is recorded in one transaction with
\* the parent's state; an exiting or dead parent refuses the start.
Link(c) ==
    /\ phase[c] = "admitted"
    /\ IF kind[c] \in {"spawn", "start"} \/ (kind[c] = "link" /\ Fault = "LinkAfterStarted")
       THEN /\ phase' = [phase EXCEPT ![c] = "linked"]
            /\ UNCHANGED <<edge, mon, linkedAt, monAt, res, place, reg>>
       ELSE IF pst = "run" \/ Fault = "LinkCheckLate"
       THEN /\ phase' = [phase EXCEPT ![c] = "linked"]
            /\ edge' = [edge EXCEPT ![c] = kind[c] = "link"]
            /\ mon' = [mon EXCEPT ![c] = kind[c] = "monitor"]
            /\ linkedAt' = IF kind[c] = "link" THEN [linkedAt EXCEPT ![c] = "before"] ELSE linkedAt
            /\ monAt' = IF kind[c] = "monitor" THEN [monAt EXCEPT ![c] = "before"] ELSE monAt
            /\ UNCHANGED <<res, place, reg>>
       ELSE /\ Abort(c, "noproc")
            /\ UNCHANGED <<linkedAt, monAt>>
    /\ UNCHANGED <<pVars, optVars, admOpen, startVars, exits, downs, div, divEver>>

\* The name is taken atomically, or the start is refused with AlreadyStarted.
Register(c) ==
    /\ phase[c] = "linked"
    /\ IF ~wname[c]
       THEN /\ phase' = [phase EXCEPT ![c] = "registered"]
            /\ UNCHANGED <<res, place, edge, mon, reg>>
       ELSE IF reg = {} \/ Fault = "DoubleRegister"
       THEN /\ phase' = [phase EXCEPT ![c] = "registered"]
            /\ reg' = reg \cup {c}
            /\ UNCHANGED <<res, place, edge, mon>>
       ELSE Abort(c, "already")
    /\ UNCHANGED <<pVars, optVars, admOpen, linkedAt, monAt, startVars, exits, downs, div, divEver>>

\* The child is handed to its executor; spawn returns Ok here, start,
\* start_link and start_monitor wait for the ack.
Dispatch(c) ==
    /\ phase[c] = "registered"
    /\ phase' = [phase EXCEPT ![c] = "starting"]
    /\ res' = [res EXCEPT ![c] = IF kind[c] = "spawn" THEN "ok" ELSE "pending"]
    /\ UNCHANGED <<pVars, optVars, place, admOpen, edge, mon, linkedAt, monAt,
                   startVars, obsVars>>

-----------------------------------------------------------------------------
(* Stage 2, on the child. *)

Exit(c, r) ==
    /\ phase' = [phase EXCEPT ![c] = "exiting"]
    /\ why' = [why EXCEPT ![c] = r]

StartedBegin(c) ==
    /\ phase[c] = "starting"
    /\ ~began[c]
    /\ ~killed[c]
    /\ began' = [began EXCEPT ![c] = TRUE]
    /\ UNCHANGED <<pVars, optVars, phase, place, admOpen, edge, mon, linkedAt,
                   monAt, retd, killed, pex, pexAbn, why, cont, msgs, early,
                   res, obsVars>>

\* Only under LinkAfterStarted: the link is set once started() is running.
LateLink(c) ==
    /\ Fault = "LinkAfterStarted"
    /\ kind[c] = "link"
    /\ began[c]
    /\ linkedAt[c] = None
    /\ phase[c] \in {"starting", "ready"}
    /\ pst = "run"
    /\ edge' = [edge EXCEPT ![c] = TRUE]
    /\ linkedAt' = [linkedAt EXCEPT ![c] = "after"]
    /\ UNCHANGED <<pVars, optVars, phase, place, admOpen, mon, monAt,
                   startVars, res, obsVars>>

\* A kill received before started() began: the child ends Killed.
KilledEarly(c) ==
    /\ phase[c] = "starting"
    /\ ~began[c]
    /\ killed[c]
    /\ Exit(c, "Killed")
    /\ UNCHANGED <<pVars, optVars, place, admOpen, edge, mon, linkedAt, monAt,
                   began, retd, killed, pex, pexAbn, cont, msgs, early, res, obsVars>>

\* started() returns Init::ready, Init::continue_with, an error or ignore
\* (the child exits Normal), or panics (or Init::stop with a non-Normal
\* reason). Held signals apply when it returns: a kill wins, then a
\* non-Normal parent exit; a Normal parent exit is ignored.
StartedEnd(c) ==
    /\ phase[c] = "starting"
    /\ began[c]
    /\ ~retd[c]
    /\ retd' = [retd EXCEPT ![c] = TRUE]
    /\ \E o \in {"ready", "continue", "panic", "error"}:
         IF killed[c] THEN /\ Exit(c, "Killed") /\ UNCHANGED <<pex, cont>>
         ELSE IF o = "panic" THEN /\ Exit(c, "Panic") /\ UNCHANGED <<pex, cont>>
         ELSE IF o = "error" THEN /\ Exit(c, "Normal") /\ UNCHANGED <<pex, cont>>
         ELSE IF pex[c] \in Abnormal /\ Fault # "DropHeld"
         THEN /\ Exit(c, pex[c])
              /\ pex' = [pex EXCEPT ![c] = None]
              /\ UNCHANGED cont
         ELSE /\ phase' = [phase EXCEPT ![c] = "ready"]
              /\ pex' = [pex EXCEPT ![c] = None]
              /\ cont' = [cont EXCEPT ![c] = o = "continue"]
              /\ UNCHANGED why
    /\ UNCHANGED <<pVars, optVars, place, admOpen, edge, mon, linkedAt, monAt,
                   began, killed, pexAbn, msgs, early, res, obsVars>>

\* Others (a peer that found the name, the caller after the ack) send.
Send(c) ==
    /\ phase[c] \in {"starting", "ready"}
    /\ msgs[c] < MaxMsgs
    /\ msgs' = [msgs EXCEPT ![c] = msgs[c] + 1]
    /\ UNCHANGED <<pVars, optVars, phase, place, admOpen, edge, mon, linkedAt,
                   monAt, began, retd, killed, pex, pexAbn, cont, early, why,
                   res, obsVars>>

\* The continuation runs first; a mailbox message only after it.
RunContinue(c) ==
    /\ phase[c] = "ready"
    /\ cont[c]
    /\ cont' = [cont EXCEPT ![c] = FALSE]
    /\ UNCHANGED <<pVars, optVars, phase, place, admOpen, edge, mon, linkedAt,
                   monAt, began, retd, killed, pex, pexAbn, msgs, early, why,
                   res, obsVars>>

Handle(c) ==
    /\ phase[c] = "ready"
    /\ msgs[c] > 0
    /\ ~cont[c] \/ Fault = "ContinueQueued"
    /\ msgs' = [msgs EXCEPT ![c] = msgs[c] - 1]
    /\ early' = [early EXCEPT ![c] = early[c] \/ cont[c]]
    /\ UNCHANGED <<pVars, optVars, phase, place, admOpen, edge, mon, linkedAt,
                   monAt, began, retd, killed, pex, pexAbn, cont, why, res, obsVars>>

\* A parent exit reaching a running child.
ApplyParentExit(c) ==
    /\ phase[c] = "ready"
    /\ pex[c] # None
    /\ pex' = [pex EXCEPT ![c] = None]
    /\ IF pex[c] \in Abnormal THEN Exit(c, pex[c]) ELSE UNCHANGED <<phase, why>>
    /\ UNCHANGED <<pVars, optVars, place, admOpen, edge, mon, linkedAt, monAt,
                   began, retd, killed, pexAbn, cont, msgs, early, res, obsVars>>

\* The child's death point D: the name is released, then the link notice
\* and the Down are taken. While the start call still waits (res pending),
\* they are diverted to it (a trapping parent's Exit, the Down) and the
\* error consumes them; a non-trapping parent still exits on a non-Normal
\* notice.
ChildDie(c) ==
    /\ phase[c] = "exiting"
    /\ phase' = [phase EXCEPT ![c] = "dead"]
    /\ reg' = IF Fault = "LateUnregister" THEN reg ELSE reg \ {c}
    /\ LET r == why[c]
           pending == res[c] = "pending"
           linkTaken == edge[c] /\ pst # "dead"
           monTaken == mon[c] /\ pst # "dead"
           kills == linkTaken /\ ~ptrap /\ r \in Abnormal /\ pst = "run"
       IN /\ exits' = IF linkTaken /\ ptrap /\ (~pending \/ Fault = "NoLinkFlush")
                      THEN exits \cup {[c |-> c, r |-> r]} ELSE exits
          /\ downs' = IF monTaken /\ (~pending \/ Fault = "NoMonitorFlush")
                      THEN downs \cup {[c |-> c, r |-> r]} ELSE downs
          /\ div' = IF pending /\ ((linkTaken /\ ptrap) \/ monTaken)
                    THEN [div EXCEPT ![c] = r] ELSE div
          /\ divEver' = IF pending /\ ((linkTaken /\ ptrap) \/ monTaken)
                        THEN [divEver EXCEPT ![c] = TRUE] ELSE divEver
          /\ pst' = IF kills THEN "exiting" ELSE pst
          /\ pwhy' = IF kills THEN r ELSE pwhy
          /\ pcause' = IF kills THEN c ELSE pcause
    /\ edge' = [edge EXCEPT ![c] = FALSE]
    /\ mon' = [mon EXCEPT ![c] = FALSE]
    /\ UNCHANGED <<pplace, ptrap, gopen, optVars, place, admOpen, linkedAt,
                   monAt, startVars, res>>

-----------------------------------------------------------------------------
(* The ack of start, start_link and start_monitor. *)

AckOk(c) ==
    /\ kind[c] \in {"start", "link", "monitor"}
    /\ res[c] = "pending"
    /\ ~killed[c]
    /\ phase[c] = "ready" \/ (Fault = "AckBeforeStarted" /\ phase[c] = "starting")
    /\ res' = [res EXCEPT ![c] = "ok"]
    /\ UNCHANGED <<pVars, optVars, phase, place, admOpen, edge, mon, linkedAt,
                   monAt, startVars, obsVars>>

\* The error is returned after the child's D (OTP: after the 'DOWN').
AckFail(c) ==
    /\ res[c] = "pending"
    /\ ~killed[c]
    /\ phase[c] = "dead" \/ (Fault = "EarlyFailAck" /\ phase[c] = "exiting")
    /\ res' = [res EXCEPT ![c] = "failed"]
    /\ div' = [div EXCEPT ![c] = None]
    /\ UNCHANGED <<pVars, optVars, phase, place, admOpen, edge, mon, linkedAt,
                   monAt, startVars, reg, exits, downs, divEver>>

\* startup_timeout: unlink and demonitor (fences), then kill.
Timeout(c) ==
    /\ TimeoutEnabled
    /\ res[c] = "pending"
    /\ phase[c] = "starting"
    /\ ~killed[c]
    /\ killed' = [killed EXCEPT ![c] = TRUE]
    /\ edge' = IF Fault = "KillBeforeUnlink" THEN edge ELSE [edge EXCEPT ![c] = FALSE]
    /\ mon' = IF Fault = "KillBeforeUnlink" THEN mon ELSE [mon EXCEPT ![c] = FALSE]
    /\ UNCHANGED <<pVars, optVars, phase, place, admOpen, linkedAt, monAt,
                   began, retd, pex, pexAbn, why, cont, msgs, early, res,
                   obsVars>>

\* Returns Timeout after the child's D or after the bounded wait for it.
AckTimeout(c) ==
    /\ res[c] = "pending"
    /\ killed[c]
    /\ res' = [res EXCEPT ![c] = "timeout"]
    /\ div' = [div EXCEPT ![c] = None]
    /\ UNCHANGED <<pVars, optVars, phase, place, admOpen, edge, mon, linkedAt,
                   monAt, startVars, reg, exits, downs, divEver>>

\* The start future is dropped before it resolves: the child keeps running
\* and stays linked or monitored; a notice already diverted goes to P as if
\* it had never been diverted.
Abandon(c) ==
    /\ res[c] = "pending"
    /\ res' = [res EXCEPT ![c] = "abandoned"]
    /\ div' = [div EXCEPT ![c] = None]
    /\ LET r == div[c]
           give == r # None /\ Fault # "AbandonDropsNotice"
       IN /\ exits' = IF give /\ kind[c] = "link" THEN exits \cup {[c |-> c, r |-> r]} ELSE exits
          /\ downs' = IF give /\ kind[c] = "monitor" THEN downs \cup {[c |-> c, r |-> r]} ELSE downs
    /\ UNCHANGED <<pVars, optVars, phase, place, admOpen, edge, mon, linkedAt,
                   monAt, startVars, reg, divEver>>

-----------------------------------------------------------------------------
(* The parent and the group. *)

PExit ==
    /\ ParentCanExit
    /\ pst = "run"
    /\ \E r \in {"Normal", "Panic"}:
         /\ pst' = "exiting"
         /\ pwhy' = r
    /\ UNCHANGED <<pplace, ptrap, pcause, gopen, optVars, phase, place, admOpen,
                   edge, mon, linkedAt, monAt, startVars, res, obsVars>>

\* P's D: each linked child gets a parent exit (held if its started() has
\* not returned); P's monitors are cancelled.
PDie ==
    /\ pst = "exiting"
    /\ pst' = "dead"
    /\ pex' = [c \in Kids |-> IF edge[c] THEN pwhy ELSE pex[c]]
    /\ pexAbn' = [c \in Kids |-> pexAbn[c] \/ (edge[c] /\ pwhy \in Abnormal)]
    /\ edge' = [c \in Kids |-> FALSE]
    /\ mon' = [c \in Kids |-> FALSE]
    /\ UNCHANGED <<pplace, ptrap, pwhy, pcause, gopen, optVars, phase, place,
                   admOpen, linkedAt, monAt, began, retd, killed, why, cont,
                   msgs, early, res, obsVars>>

GClose ==
    /\ CloseEnabled
    /\ gopen
    /\ gopen' = FALSE
    /\ UNCHANGED <<pst, pplace, ptrap, pwhy, pcause, optVars, phase, place,
                   admOpen, edge, mon, linkedAt, monAt, startVars, res, obsVars>>

ChildStep(c) ==
    \/ Begin(c) \/ Admit(c) \/ Link(c) \/ Register(c) \/ Dispatch(c)
    \/ StartedBegin(c) \/ LateLink(c) \/ KilledEarly(c) \/ StartedEnd(c)
    \/ ApplyParentExit(c) \/ ChildDie(c)
    \/ Send(c) \/ RunContinue(c) \/ Handle(c)
    \/ AckOk(c) \/ AckFail(c) \/ Timeout(c) \/ AckTimeout(c) \/ Abandon(c)

Next == (\E c \in Kids : ChildStep(c)) \/ PExit \/ PDie \/ GClose

\* Liveness assumes the child's own steps and P's death complete.
Progress(c) ==
    Register(c) \/ Dispatch(c) \/ StartedBegin(c) \/ KilledEarly(c)
    \/ StartedEnd(c) \/ ApplyParentExit(c) \/ ChildDie(c)
Fairness == /\ \A c \in Kids : WF_vars(Progress(c)) /\ WF_vars(Link(c)) /\ WF_vars(Admit(c))
            /\ WF_vars(PDie)

Spec == Init /\ [][Next]_vars
LiveSpec == Spec /\ Fairness

-----------------------------------------------------------------------------
(* Safety. *)

\* Explicit placement wins; a linked or monitored child takes its parent's
\* placement; a plain start is unmanaged.
InheritedPlacement == \A c \in Kids : place[c] # None => place[c] = Expected(c)

\* The link or monitor exists before started() begins.
LinkBeforeStarted ==
    \A c \in Kids :
        /\ (began[c] /\ kind[c] = "link") => linkedAt[c] = "before"
        /\ (began[c] /\ kind[c] = "monitor") => monAt[c] = "before"

NameUnique == Cardinality(reg) <= 1

\* The name is free once the holder is past D (and before any notice,
\* Down or failure result is observable).
NameReleasedAtDeath == \A c \in reg : phase[c] \notin {"dead", "aborted"}

\* A refused start leaves no trace.
AbortInvisible ==
    \A c \in Kids : phase[c] = "aborted" =>
        /\ ~edge[c] /\ ~mon[c] /\ c \notin reg /\ ~began[c] /\ place[c] = None
        /\ \A e \in exits \cup downs : e.c # c
        /\ pcause # c

\* Ok from start, start_link or start_monitor means started() returned.
AckMeansStarted ==
    \A c \in Kids : (res[c] = "ok" /\ kind[c] \in {"start", "link", "monitor"}) => retd[c]

\* A notice diverted to a start call that was then dropped reaches P.
AbandonedNoticeDelivered ==
    \A c \in Kids : (res[c] = "abandoned" /\ divEver[c]) =>
        \E e \in exits \cup downs : e.c = c

\* No mailbox message is handled before the continuation.
ContinueFirst == \A c \in Kids : ~early[c]

\* A startup failure is returned after the child's D.
FailureAfterDeath == \A c \in Kids : res[c] = "failed" => phase[c] = "dead"

\* A failed or timed-out start_monitor delivers no Down; a failed or
\* timed-out start_link delivers no Exit to a trapping parent.
FailureFlushed ==
    \A c \in Kids : res[c] \in {"failed", "timeout"} =>
        /\ kind[c] = "monitor" => \A d \in downs : d.c # c
        /\ (kind[c] = "link" /\ ptrap) => \A e \in exits : e.c # c

\* A startup-timeout kill never reaches the parent.
TimeoutContained ==
    \A c \in Kids : killed[c] =>
        /\ pcause # c
        /\ \A e \in exits \cup downs : e.c # c

\* No link or monitor edge survives the parent's D.
NoEdgeToDeadParent == \A c \in Kids : (edge[c] \/ mon[c]) => pst # "dead"

AdmittedOnlyOpen == \A c \in Kids : place[c] = "G" => admOpen[c]

CapacityRespected == LiveInG <= Cap

NoticeOnce ==
    \A c \in Kids :
        /\ Cardinality({e \in exits : e.c = c}) <= 1
        /\ Cardinality({d \in downs : d.c = c}) <= 1

Safety ==
    /\ InheritedPlacement /\ LinkBeforeStarted /\ NameUnique
    /\ NameReleasedAtDeath /\ AbortInvisible /\ AckMeansStarted
    /\ FailureAfterDeath /\ FailureFlushed /\ TimeoutContained
    /\ NoEdgeToDeadParent /\ AdmittedOnlyOpen /\ CapacityRespected /\ NoticeOnce
    /\ ContinueFirst /\ AbandonedNoticeDelivered

(* Liveness: a child linked to a parent that dies abnormally, also during   *)
(* the child's started(), dies.                                            *)
ParentExitReachesChild ==
    \A c \in Kids : pexAbn[c] ~> phase[c] \in {"dead", "aborted"}
=============================================================================
