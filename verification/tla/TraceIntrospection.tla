------------------------- MODULE TraceIntrospection -------------------------
(***************************************************************************)
(* Trace validation for Introspection.tla: one actor's activities and its *)
(* status turn (sys:get_state / sys:get_status; design                     *)
(* docs/design/introspection.md, "Trace events and their mapping").        *)
(*                                                                         *)
(* With the `lifecycle-trace` feature, while `record_introspection` is on, *)
(* the code records one event per model action for each actor spawned     *)
(* (concurrency/src/lifecycle_trace.rs, IntroEvent), each inside the       *)
(* critical section of the state change it names (the trace section is     *)
(* the outermost lock), so the recorded order is a linearization:          *)
(*   StartEnd, StartPanic   started() returned ready or panicked, in the   *)
(*              step of the startup decision                               *)
(*   Send       a message admitted ("queued") or refused ("noproc")        *)
(*   Status     a status request admitted or refused (SendStatus)          *)
(*   Dispatch   the run loop takes a "msg" or a "status" envelope, in the  *)
(*              step of its stop check                                     *)
(*   Admit      the status envelope's slot check: "run" or "skip"          *)
(*   Answer     the status reply's discharge: "ok" or "failed" (a caught   *)
(*              callback panic)                                            *)
(*   End        a turn returned (MsgEnd, StatusEnd)                        *)
(*   Panic      a handler panicked (MsgPanic)                              *)
(*   Exit       the exit decision: stopped() "run" or "skip" (ExitDecide) *)
(*   HookBegin, HookEnd, HookPanic   stopped()                             *)
(*   Stop, Kill   an administrative or self stop, a kill (Stop,           *)
(*              ExternalKill; a repeated one changes nothing)              *)
(*   Drop       the runtime dropped the actor before D (DropSwap)          *)
(*   Die        the death point with the final reason class; every queued  *)
(*              status request is answered there (Die)                     *)
(*   Publish    publication                                                *)
(*                                                                         *)
(* Unlogged: the status caller's deadline or a dropped receiver closing    *)
(* the slot (StatusTimeout, a silent step; the Admit or Die that follows   *)
(* tells whether it happened). Handlers are atomic: Suspend and Resume are *)
(* not taken. Not in the mapping (the trace is rejected): exit signals     *)
(* other than a kill, trapped exits, links and monitors (Lifecycle.tla),   *)
(* a start that fails without a panic (an init error, stop or ignore, the *)
(* init timer, a kill before started() began) and a continuation turn     *)
(* (Start.tla), more than one status request, and the watchdog (not      *)
(* implemented).                                                           *)
(*                                                                         *)
(* Reason classes: a stop is the model's Shutdown (stopReq) whatever its   *)
(* reason; Panic; Killed. After a drop the reason is not compared (the     *)
(* code turns a Normal stop into Killed, the model keeps the class).       *)
(* verification/tla/trace_check.py (schema introspection-v1) generates the *)
(* constants and runs TLC.                                                 *)
(***************************************************************************)
EXTENDS Introspection, TLC

CONSTANT Trace

VARIABLE i

traceVars == <<vars, i>>

TraceInit == Init /\ i = 0

\* The model action named by logged event e, constrained by its outcome.
Step(e) ==
    CASE e.a = "StartEnd"   -> StartedEnd
      [] e.a = "StartPanic" -> StartPanic
      [] e.a = "Send"       -> Send /\ (e.x = "noproc") = Past
      [] e.a = "Status"     -> SendStatus /\ st' = (IF e.x = "noproc" THEN "noproc" ELSE "waiting")
      [] e.a = "Dispatch"   -> Dispatch /\ kind' = e.x
      [] e.a = "Admit"      -> StatusAdmit /\ stAdm' = e.x
      [] e.a = "Answer"     -> StatusAnswer /\ stFailed' = (e.x = "failed")
      [] e.a = "End"        -> IF e.x = "status" THEN StatusEnd ELSE MsgEnd
      [] e.a = "Panic"      -> MsgPanic
      [] e.a = "Exit"       -> ExitDecide /\ (ph' = "hookwait") = (e.x = "run")
      [] e.a = "HookBegin"  -> HookBegin
      [] e.a = "HookEnd"    -> HookEnd
      [] e.a = "HookPanic"  -> HookPanic
      [] e.a = "Drop"       -> DropSwap
      [] e.a = "Die"        -> Die /\ (ph = "dropping" \/ reason' = e.x)
      [] e.a = "Publish"    -> Publish
      [] OTHER              -> FALSE

\* Environment events that may repeat in the code: a second stop or kill
\* changes nothing (the model's Stop and ExternalKill take the first).
Repeated(e) ==
    \/ e.a = "Stop" /\ (stopReq \/ Past)
    \/ e.a = "Kill" /\ (killed \/ Past)

Env(e) ==
    \/ e.a = "Stop" /\ ~stopReq /\ ~Past /\ Stop
    \/ e.a = "Kill" /\ ~killed /\ ~Past /\ ExternalKill

TraceNext ==
    /\ i < Len(Trace)
    /\ \/ (Step(Trace[i + 1]) \/ Env(Trace[i + 1])) /\ Ghosts /\ i' = i + 1
       \/ Repeated(Trace[i + 1]) /\ UNCHANGED vars /\ i' = i + 1
       \/ StatusTimeout /\ Ghosts /\ UNCHANGED i

TraceSpec == TraceInit /\ [][TraceNext]_traceVars

\* Violated exactly when the whole log has been matched: TLC reporting this
\* invariant as violated means the trace is ACCEPTED.
TraceIncomplete == i < Len(Trace)
=============================================================================
