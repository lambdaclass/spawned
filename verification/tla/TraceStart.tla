------------------------------ MODULE TraceStart ------------------------------
(***************************************************************************)
(* Trace validation for Start.tla (unified start, 0.9; design              *)
(* docs/design/unified-start.md, 16.2).                                    *)
(*                                                                         *)
(* With the `lifecycle-trace` feature, spawned-concurrency records one     *)
(* event per model action for the starts made while `record_start` is on   *)
(* (concurrency/src/lifecycle_trace.rs, StartEvent), at the code's          *)
(* linearization point of the state change it describes:                   *)
(*   SBegin       entry of a start verb (kind, explicit placement, named)  *)
(*   SPrepareFail invalid options, before anything exists (PrepareFail)   *)
(*   SCommit      the stage-1 commit, all its locks held (Commit)         *)
(*   SRefuse      a stage-1 refusal decided under the same locks (Commit) *)
(*   SLookup      Name::whereis under the registry lock (Lookup; a miss is *)
(*                a stuttering step that requires the name free)           *)
(*   SStartedBegin  before started() is first polled (StartedBegin); also *)
(*                for a managed child whose admission closed before it:   *)
(*                started() is skipped and the start resolves as row 9    *)
(*   SAdminStop, SKill  an administrative stop or kill arriving (exit-    *)
(*                request lock: AdminStop, OutsideKill)                    *)
(*   SResolve     the startup decision under the exit-request and cell    *)
(*                locks (StartedEnd; KilledEarly when started() never ran) *)
(*   STimer       the init timer under the cell lock (Timeout; "lost" is  *)
(*                a stuttering step that requires a commitment)            *)
(*   SDeadline    the waiter's cleanup deadline (CleanupDeadline)         *)
(*   SSend, SHandle, SContinue  admission of a message, its dispatch, the *)
(*                continuation's turn (Send, Handle, RunContinue)          *)
(*   SExit        a ready child's exit begins (ReadyExit, KillReady,      *)
(*                ApplyParentExit, HandleQueuedExit; or AdminStopApply:   *)
(*                an administrative stop held during started(), recorded *)
(*                right after the ready SResolve)                          *)
(*   SUnlink      the child unlinks from P (ChildUnlink)                  *)
(*   SDie         D (Die); f = TRUE: the D of a non-ready decision, which *)
(*                StartedEnd already took (a stuttering step whose reason  *)
(*                must match)                                              *)
(*   SReleaseName the name's release under the registry lock             *)
(*   SPublish     the notice disposition under the cell lock (Publish)    *)
(*   SAnswer      the waiter takes its answer (AckOk, AckFail)            *)
(*   SReady       ready() returns (a stuttering step that requires the    *)
(*                matching commitment)                                     *)
(*   SAbandon     the Starting is dropped unanswered (Abandon)            *)
(*   SWorkDone, SReleaseSlot  the scope's completion: owned work joined,  *)
(*                then the slot released under the group lock              *)
(*   PExitBegin, PDie, PPublish  the parent's exit begin (not one begun  *)
(*                by a child's notice), D and publication                  *)
(*   PNotice      a child's notice enqueued to or applied on P (Deliver)  *)
(*                                                                         *)
(* Mapping notes:                                                          *)
(*  - ReleaseName of an unnamed child changes nothing observable and has  *)
(*    no event: it is taken silently. So is WorkDoneP.                     *)
(*  - The code decides a notice's disposition at publication whether or   *)
(*    not P is alive; the model only for a live P. SPublish's "divert" is *)
(*    checked against the model where P's mailbox admits.                  *)
(*  - Not in the mapping: scope ownership (S8: OwnerClose, the scope      *)
(*    deadline, GenDone; ScopeEnabled = FALSE), group closure, and a      *)
(*    second group G2.                                                     *)
(*                                                                         *)
(* The trace is accepted iff some behavior consumes the whole log         *)
(* (TraceIncomplete violated). verification/tla/trace_check.py (schema    *)
(* start-v1) generates the constants and runs TLC.                         *)
(***************************************************************************)
EXTENDS Start, Sequences, TLC

CONSTANTS Trace, TracePlace, TraceTrap

VARIABLE i

traceVars == <<vars, i>>

TraceInit == Init /\ pplace = TracePlace /\ ptrap = TraceTrap /\ i = 0

Kind(e) == e.x
ExplicitOf(e) == IF e.y = "none" THEN None ELSE e.y

BeginAs(e) ==
    /\ Begin(e.c)
    /\ kind'[e.c] = e.x /\ expl'[e.c] = ExplicitOf(e) /\ wname'[e.c] = e.f

CommitAs(e) ==
    /\ Commit(e.c)
    /\ IF e.a = "SCommit"
       THEN phase'[e.c] = "starting" /\ place'[e.c] = e.x
       ELSE phase'[e.c] = "aborted" /\ res'[e.c] = e.x

LookupAs(e) ==
    IF e.x = "hit" THEN Lookup /\ seen'[e.c] /\ e.c \in reg
    ELSE reg = {} /\ UNCHANGED vars

\* The outcome code maps to the model's: a closed admission that skipped
\* started() resolves as "ready".
Outcome(e) == IF e.x = "none" THEN "ready" ELSE e.x

\* What started() returned, as the model's outcomes: the recorded code
\* (`e.x`) pins it. A `ctx.stop*` called inside a started() that returned
\* ready is the model's stop outcome (row 5), whose reason `e.z` pins.
Returns(e) ==
    IF e.y = "stop" /\ e.x \in {"ready", "continue"}
    THEN {"stopN", "stopS"}
    ELSE {Outcome(e)}

ResolveAs(e) ==
    IF e.x = "none" /\ e.y \in {"kill", "timer"}
    THEN KilledEarly(e.c)
    ELSE /\ StartedEndOf(e.c, Returns(e))
         /\ ctrap'[e.c] = e.f
         /\ IF e.y = "ready"
            THEN /\ phase'[e.c] = "ready"
                 /\ cont'[e.c] = (Outcome(e) = "continue" /\ ~astop[e.c])
            ELSE /\ phase'[e.c] = "dead"
                 /\ (e.z # "" => why'[e.c] = e.z)
                 /\ commit0'[e.c] = CASE e.y = "ignore" -> "ignored"
                                       [] e.y = "timer" -> "timedout"
                                       [] OTHER -> commit0'[e.c]

TimerAs(e) ==
    IF e.x = "won" THEN Timeout(e.c)
    ELSE commit[e.c] # None /\ UNCHANGED vars

ExitAs(e) ==
    /\ \/ ReadyExit(e.c) \/ KillReady(e.c) \/ ApplyParentExit(e.c) \/ HandleQueuedExit(e.c)
       \/ AdminStopApply(e.c)
    /\ phase'[e.c] = "exiting" /\ why'[e.c] = e.x

DieAs(e) ==
    IF e.f THEN phase[e.c] = "dead" /\ why[e.c] = e.x /\ UNCHANGED vars
    ELSE Die(e.c) /\ why[e.c] = e.x

\* The recorded disposition must be the model's: a diverted notice is
\* "divert", a notice the model hands to P is "deliver", and "drop" neither
\* diverts nor delivers. (For a dead P the code still records its decision;
\* the model has nothing to deliver, so only the implications below apply.)
PublishAs(e) ==
    /\ Publish(e.c)
    /\ (div'[e.c] # None) => e.x = "divert"
    /\ (e.x = "divert" /\ Alive(pst) /\ (lnote[e.c] \/ mnote[e.c])) => div'[e.c] # None
    /\ (obN'[e.c] > obN[e.c]) => e.x = "deliver"
    /\ e.x = "drop" => (obN'[e.c] = obN[e.c] /\ div'[e.c] = None)

AnswerAs(e) ==
    IF e.x = "ok" THEN AckOk(e.c)
    ELSE /\ AckFail(e.c)
         /\ res'[e.c] = CASE e.x = "ignored" -> "ignored"
                          [] e.x = "timeout" -> "timeout"
                          [] OTHER -> "failed"
         /\ pend'[e.c] = e.f
         /\ (e.x = "failed" /\ e.z # "") => resR'[e.c] = e.z

ReadyAs(e) ==
    /\ CASE e.x = "ok" -> commit0[e.c] = "ready"
         [] e.x = "ignored" -> commit0[e.c] = "ignored"
         [] e.x = "timeout" -> commit0[e.c] = "timedout"
         [] OTHER -> commit0[e.c] = "failed"
    /\ e.x # "ok" => (phase[e.c] = "published" \/ dl[e.c])
    /\ UNCHANGED vars

AbandonAs(e) == Abandon(e.c) /\ (handed'[e.c] => e.f)

NoticeAs(e) ==
    /\ Deliver(e.c)
    /\ obR[e.c] = e.z
    /\ (e.x = "monitor") = (kind[e.c] = "monitor")

PExitAs(e) == PExit /\ pwhy' = e.x

\* The model action named by logged event e, constrained by its parameters.
Step(e) ==
    CASE e.a = "SBegin"        -> BeginAs(e)
      [] e.a = "SPrepareFail"  -> PrepareFail(e.c)
      [] e.a = "SCommit"       -> CommitAs(e)
      [] e.a = "SRefuse"       -> CommitAs(e)
      [] e.a = "SLookup"       -> LookupAs(e)
      [] e.a = "SStartedBegin" -> StartedBegin(e.c)
      [] e.a = "SAdminStop"    -> AdminStop(e.c)
      [] e.a = "SKill"         -> OutsideKill(e.c)
      [] e.a = "SResolve"      -> ResolveAs(e)
      [] e.a = "STimer"        -> TimerAs(e)
      [] e.a = "SDeadline"     -> CleanupDeadline(e.c)
      [] e.a = "SSend"         -> Send(e.c)
      [] e.a = "SHandle"       -> Handle(e.c)
      [] e.a = "SContinue"     -> RunContinue(e.c)
      [] e.a = "SExit"         -> ExitAs(e)
      [] e.a = "SUnlink"       -> ChildUnlink(e.c)
      [] e.a = "SDie"          -> DieAs(e)
      [] e.a = "SReleaseName"  -> ReleaseName(e.c)
      [] e.a = "SPublish"      -> PublishAs(e)
      [] e.a = "SAnswer"       -> AnswerAs(e)
      [] e.a = "SReady"        -> ReadyAs(e)
      [] e.a = "SAbandon"      -> AbandonAs(e)
      [] e.a = "SWorkDone"     -> WorkDoneC(e.c)
      [] e.a = "SReleaseSlot"  -> ReleaseSlot(e.c)
      [] e.a = "PExitBegin"    -> PExitAs(e)
      [] e.a = "PDie"          -> PDie
      [] e.a = "PPublish"      -> PPublish
      [] e.a = "PNotice"       -> NoticeAs(e)
      [] OTHER                 -> FALSE

\* Unlogged steps: an unnamed child's name release, P's owned work.
Silent ==
    \/ \E c \in Kids : ~wname[c] /\ ReleaseName(c)
    \/ WorkDoneP

TraceNext ==
    /\ i < Len(Trace)
    /\ \/ Step(Trace[i + 1]) /\ i' = i + 1
       \/ Silent /\ UNCHANGED i

TraceSpec == TraceInit /\ [][TraceNext]_traceVars

\* All the model's invariants hold along the trace.
Safety ==
    /\ TypeOK /\ InheritedPlacement /\ LinkBeforeStarted /\ EdgeOnlyToRunningParent
    /\ NoEdgeToDeadParent /\ NameUnique /\ NameHeldUntilReleased /\ NameKeptWhileLive
    /\ NoOverlapOnReuse /\ NameFreeBeforeObservation /\ AbortInvisible /\ CommitImmutable
    /\ ResultMatchesCommit /\ ReadyIsFinal /\ ReadyReturnCommitsReady /\ TimeoutNeverKillsReady /\ ReadyOnlyWhileOpen
    /\ FailureAfterPublication /\ ReportedReasonFinal /\ PendingOnlyAfterDeadline
    /\ KillBeforeDIsKilled /\ IgnoredExitsNormal /\ FailureFlushed
    /\ FailureEndsNonTrappingParent /\ NormalFailureSparesParent /\ TimeoutContained
    /\ AdmittedOnlyOpen /\ OwnedOnlyWhileOwnerOpen /\ CapacityRespected /\ ParentSlotHeld
    /\ SlotHeldUntilReleased /\ NoticeOnce /\ NoLostNotice /\ AbandonedNoticeDelivered
    /\ ContinueFirst /\ OwnerRefusalOnlyOnShutdown

\* Violated exactly when the whole log has been matched and nothing the
\* model owes a live P is still undelivered: TLC reporting this invariant
\* as violated means the trace is ACCEPTED. (A recording ends after the
\* notices it waits for, so a disposed notice never delivered is a lost one.)
TraceIncomplete ==
    \/ i < Len(Trace)
    \/ \E c \in Kids : obN[c] > 0 /\ Alive(pst)
=============================================================================
