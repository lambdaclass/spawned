-------------------------- MODULE TraceSupervision --------------------------
(***************************************************************************)
(* Trace validation for Supervision.tla.                                   *)
(*                                                                         *)
(* With the `lifecycle-trace` feature, spawned-concurrency records one     *)
(* event per model action (concurrency/src/lifecycle_trace.rs). Each event *)
(* is a record [a |-> action, c |-> child (0 = none), x |-> parameter,     *)
(* n |-> retry budget held]. TraceNext replays the log: step i+1 must be   *)
(* the named model action with matching parameters. The trace is accepted  *)
(* iff some behavior of the model consumes the whole log, i.e. TLC finds a *)
(* state with i = Len(Trace) (reported as a violation of TraceIncomplete). *)
(*                                                                         *)
(* Unlogged steps: the rolling restart window has no observable event, so  *)
(* WindowExpire may stutter (i unchanged) before a BackoffDone, whose      *)
(* logged budget count pins the window's content. Exit reasons the code    *)
(* could not observe (cleanup stuck before the actor finished) are logged  *)
(* as "unknown" and matched against every reason.                          *)
(*                                                                         *)
(* verification/tla/trace_check.py generates the Trace, N and Mode         *)
(* constants from a recorded JSON trace and runs TLC.                      *)
(***************************************************************************)
EXTENDS Supervision, TLC

CONSTANT Trace

VARIABLE i

traceVars == <<vars, i>>

TraceInit == Init /\ i = 0

Bool(x) == x = "true"

ChildEvents == {"RetryReserve", "RetryRelease", "LoopTop", "BackoffDone", "Attempt", "BecomeReady",
                "StartupFailed", "SelfExit", "StopObserved", "CleanupDone",
                "CleanupStuck", "DeferExit", "Undefer", "Decide", "Finish"}

\* The model action named by logged event e, constrained by its parameters.
Step(e) ==
    LET c == e.c IN
    CASE e.a = "Close"            -> Close
      [] e.a = "ShutdownDeadline" -> ShutdownDeadline
      [] e.a = "SupLaunch"        -> sidx = c /\ SupLaunch
      [] e.a = "SupStartDone"     -> SupStartDone /\ sready' = Bool(e.x)
      [] e.a = "SupCancelSeen"    -> SupCancelSeen
      [] e.a = "SupStopTurn"      -> sidx = c /\ SupStopTurn
      [] e.a = "SupChildDone"     -> sidx = c /\ SupChildDone
      [] e.a = "SupChildTimeout"  -> sidx = c /\ SupChildTimeout
      [] e.a = "SupJoin"          -> SupJoin
      [] e.a \in ChildEvents /\ c \in Children ->
           CASE e.a = "LoopTop"      -> LoopTop(c) /\ pc'[c] = e.x
             [] e.a = "RetryReserve" -> RetryReserve(c) /\ budget' = e.n
             [] e.a = "RetryRelease" -> RetryRelease(c) /\ budget' = e.n
             [] e.a = "BackoffDone"  ->
                  /\ BackoffDone(c)
                  /\ CASE e.x = "closed"    -> ~open
                       [] e.x = "retry"     -> open /\ pc'[c] = "attempt" /\ budget' = e.n
                       [] e.x = "exhausted" -> open /\ ~open' /\ budget = e.n
                       [] OTHER             -> FALSE
             [] e.a = "Attempt"      -> Attempt(c) /\ open = Bool(e.x)
             [] e.a = "BecomeReady"  -> BecomeReady(c) /\ open = Bool(e.x)
             [] e.a = "StartupFailed" ->
                  IF e.x = "unknown"
                  THEN \E r \in StartupResults : StartupFailed(c, r)
                  ELSE StartupFailed(c, e.x)
             [] e.a = "SelfExit"     ->
                  IF e.x = "unknown"
                  THEN (\E r \in SelfExits : SelfExit(c, r)) \/ StopObserved(c)
                  ELSE SelfExit(c, e.x)
             [] e.a = "StopObserved" -> StopObserved(c)
             [] e.a = "CleanupDone"  -> CleanupDone(c)
             [] e.a = "CleanupStuck" -> CleanupStuck(c)
             [] e.a = "DeferExit"    -> DeferExit(c)
             [] e.a = "Undefer"      -> Undefer(c)
             [] e.a = "Decide"       ->
                  /\ Decide(c)
                  /\ CASE e.x = "closed"  -> ~open
                       [] e.x = "restart" -> open /\ pc'[c] = "top"
                       [] e.x = "stop"    -> open /\ open' /\ pc'[c] = "finish"
                       [] e.x = "fail"    -> open /\ ~open'
                       [] OTHER           -> FALSE
             [] e.a = "Finish"       -> Finish(c)
             [] OTHER                -> FALSE
      [] OTHER -> FALSE   \* unknown or "Unmodelled" events reject the trace

TraceNext ==
    /\ i < Len(Trace)
    /\ \/ Step(Trace[i + 1]) /\ i' = i + 1
       \/ Trace[i + 1].a \in {"BackoffDone", "RetryReserve"} /\ WindowExpire /\ UNCHANGED i

TraceSpec == TraceInit /\ [][TraceNext]_traceVars

\* Violated exactly when the whole log has been matched: TLC reporting this
\* invariant as violated means the trace is ACCEPTED.
TraceIncomplete == i < Len(Trace)
=============================================================================
