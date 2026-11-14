--------------------------- MODULE TraceLifecycle ---------------------------
(***************************************************************************)
(* Trace validation for Lifecycle.tla (rank 5e exit signals, 5g runtime    *)
(* drop; design docs/design/otp-exit-signals.md).                          *)
(*                                                                         *)
(* With the `lifecycle-trace` feature, spawned-concurrency records, for    *)
(* the actors spawned while recording is on, one event per model action    *)
(* (concurrency/src/lifecycle_trace.rs, LifeEvent). Each event is recorded *)
(* inside the critical section of the state change it describes, under    *)
(* one process-wide trace section, so the recorded order is a              *)
(* linearization:                                                          *)
(*   Trap      ctx.trap_exit, when the flag changes (SetTrap)              *)
(*   Request,  a send or request admitted ("queued") or refused ("noproc") *)
(*   Enqueue   (one recorded Send; trace_check.py splits it in two)        *)
(*   Send      a plain message from a recorded actor's turn or hook to     *)
(*             another, admitted (Send; its Dispatch has kind "msg")       *)
(*   Dispatch  the run loop takes the mailbox head of kind k, in the same  *)
(*             step as its stop check (Dispatch); p: the turn of a request *)
(*             panicked (the model's panic branch)                         *)
(*   Panic     a panic in a turn, started() or stopped() (SelfExit or      *)
(*             PanicWhileExiting); p: already taken by its Dispatch        *)
(*   Stop      ctx.stop*, ChildHandle::stop* (SelfExit or AdminStop)       *)
(*   Signal    ctx.exit, ChildHandle::exit/kill (ExitSignal), or a         *)
(*             supervisor's Shutdown or kill (SupShutdown, SupKill)        *)
(*   Link      link/try_link that installed a record or found the peer     *)
(*             past D (Link, TryLink); Unlink, Monitor, Demonitor          *)
(*   Deliver   a link notice or Down applied by its receiver (Deliver)     *)
(*   Hook      stopped() begins ("run") or is skipped ("skip")             *)
(*   Die       the death point D with the final reason (Die)               *)
(*   Drop      the runtime dropped the actor before D (RuntimeDrop)        *)
(*   Publish   destruction finished; link notices are delivered after it  *)
(*             and wait_exit returns after them (Publish)                  *)
(*                                                                         *)
(* Identities: the recorded actors are a1, a2, ... in spawn order (their   *)
(* start_linked parents give LinkParent; the model starts them all, idle,  *)
(* and a Spawn event is not replayed). Outside code is Ext; a supervisor's *)
(* signal is Sup and its target is in Children. A plain message that a    *)
(* recorded actor sends to another during a turn or hook (the recorder     *)
(* tracks the actor whose turn a thread runs) is the model's Send; every   *)
(* other send or request (from outside code, an unrecorded actor, to       *)
(* itself, a request, or refused) is a request from Ext.                  *)
(*                                                                         *)
(* Mapping notes:                                                          *)
(*  - Handlers are atomic in the model. Dispatch is recorded when the      *)
(*    message is taken, and a handler's own actions (Stop, Link, Monitor,  *)
(*    ...) follow it as separate steps of a running actor. A request turn  *)
(*    that panics is the model's panic branch of Dispatch only when the    *)
(*    actor did nothing else in between; otherwise its Dispatch is the     *)
(*    reply branch and the Panic a SelfExit.                               *)
(*  - A parent exit is the model's Dispatch (which begins exit); the stop  *)
(*    it requests in code is not recorded.                                 *)
(*  - A non-trapping caller of link to an actor past D exits NoProc in the *)
(*    model's Link step; the code applies the late notice right after, in  *)
(*    the same section, so that Deliver is a stutter step here.            *)
(*  - Hook "skip" is also accepted when the model's hook ghost is TRUE but *)
(*    a kill arrived before stopped() began: the code then skips stopped() *)
(*    (E1), while Lifecycle.tla's Kill during "stopping" leaves hook as it *)
(*    was. Hook "run" requires the ghost (SkipHook).                       *)
(*  - A panic after a kill, or a second panic, changes nothing in the      *)
(*    model's reason classes and is a stutter step.                        *)
(*  - Not in the mapping (an event the model cannot take rejects the       *)
(*    trace): phase 0 (signals held while started() runs, recorded as      *)
(*    Held), managed scopes' own stops, actor-to-actor requests, time.     *)
(*                                                                         *)
(* There are no unlogged steps: every model step is a logged event, so     *)
(* TLC replays a trace almost deterministically. The trace is accepted iff *)
(* some behavior consumes the whole log (TraceIncomplete violated).        *)
(* verification/tla/trace_check.py (schema lifecycle-v1) generates the     *)
(* constants and runs TLC.                                                 *)
(***************************************************************************)
EXTENDS Lifecycle, TLC

CONSTANT Trace

VARIABLE i

traceVars == <<vars, i>>

TraceInit == Init /\ i = 0

\* Await-free generated handlers may complete inside guarded dispatch, before
\* creating a turn future. They still run after this Dispatch action, with the
\* same current-actor context used for Send. lifecycle_direct_dispatch.json
\* exercises the implementation path (including the causal sender).
DispatchAs(a, k, p) ==
    /\ mbox[a] # <<>> /\ Head(mbox[a]).k = k
    /\ Dispatch(a)
    /\ CASE k = "req"  -> req'[Head(mbox[a]).q].s = IF p THEN "exiting" ELSE "ok"
         \* exit_received's own stop, if any, is a later Stop event
         [] k = "exit" -> UNCHANGED life
         [] OTHER      -> TRUE

PanicAs(a, folded) ==
    IF folded THEN st[a] = "stopping" /\ why[a] = "Panic" /\ UNCHANGED vars
    ELSE IF st[a] = "run" THEN SelfExit(a, "Panic")
    ELSE IF st[a] = "stopping" /\ (killed[a] \/ why[a] = "Panic") THEN UNCHANGED vars
    ELSE PanicWhileExiting(a)

SignalAs(e) ==
    IF e.s = Sup
    THEN IF e.x = "Kill" THEN SupKill(e.t) ELSE e.x = "Shutdown" /\ SupShutdown(e.t)
    ELSE ExitSignal(e.s, e.t, e.x)

LinkAs(e) ==
    CASE e.x = "try"    -> TryLink(e.t, e.s)
      [] e.x = "linked" -> ~IsGone(e.s) /\ Link(e.t, e.s)
      [] e.x = "noproc" -> IsGone(e.s) /\ Link(e.t, e.s)
      [] OTHER          -> FALSE

Matches(n, e) ==
    /\ n.to = e.t /\ n.from = e.s /\ n.k = e.k /\ n.r = e.x /\ n.o = e.o
    /\ (e.k = "down" => n.m = e.m)

DeliverAs(e) ==
    \/ \E n \in inflight : Matches(n, e) /\ Deliver(n)
    \* The late NoProc notice of a non-trapping caller: applied by Link.
    \/ /\ e.k = "link" /\ e.o = "late" /\ e.x = "NoProc"
       /\ ~\E n \in inflight : Matches(n, e)
       /\ ~trap[e.t] /\ st[e.t] = "stopping" /\ cause[e.t] = "noproc"
       /\ UNCHANGED vars

HookAs(a, x) ==
    /\ st[a] = "stopping"
    /\ IF x = "run" THEN hook[a] ELSE ~hook[a] \/ killed[a]
    /\ UNCHANGED vars

\* The model action named by logged event e, constrained by its parameters.
Step(e) ==
    CASE e.a = "Trap"      -> SetTrap(e.t, e.x = "true")
      [] e.a = "Request"   -> Request(e.q, e.t)
      [] e.a = "Enqueue"   -> Enqueue(e.q) /\ req'[e.q].s = e.x
      [] e.a = "Send"      -> Send(e.s, e.t)
      [] e.a = "Dispatch"  -> DispatchAs(e.t, e.k, e.p)
      [] e.a = "Panic"     -> PanicAs(e.t, e.p)
      [] e.a = "Stop"      -> SelfExit(e.t, e.x) \/ AdminStop(e.t, e.x)
      [] e.a = "Signal"    -> SignalAs(e)
      [] e.a = "Link"      -> LinkAs(e)
      [] e.a = "Unlink"    -> Unlink(e.t, e.s)
      [] e.a = "Monitor"   -> Monitor(e.t, e.s, e.m) /\ mstate'[e.m] = e.x
      [] e.a = "Demonitor" -> Demonitor(e.t, e.m)
      [] e.a = "Deliver"   -> DeliverAs(e)
      [] e.a = "Hook"      -> HookAs(e.t, e.x)
      [] e.a = "Die"       -> why[e.t] = e.x /\ Die(e.t)
      [] e.a = "Drop"      -> RuntimeDrop(e.t) /\ why'[e.t] = e.x
      [] e.a = "Publish"   -> Publish(e.t)
      [] OTHER             -> FALSE

\* Answering the queued requests after D is not logged: it is taken
\* silently between Die (or Drop) and Publish, which waits for it.
TraceNext ==
    /\ i < Len(Trace)
    /\ \/ Step(Trace[i + 1]) /\ i' = i + 1
       \/ \E a \in Actors : Discharge(a) /\ UNCHANGED i

TraceSpec == TraceInit /\ [][TraceNext]_traceVars

\* Violated exactly when the whole log has been matched: TLC reporting this
\* invariant as violated means the trace is ACCEPTED.
TraceIncomplete == i < Len(Trace)
=============================================================================
