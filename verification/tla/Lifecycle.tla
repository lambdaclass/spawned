----------------------------- MODULE Lifecycle -----------------------------
EXTENDS Naturals, Sequences, FiniteSets
(***************************************************************************)
(* Base actor lifecycle with exit signals as in OTP (ROADMAP ranks 5e and  *)
(* 5g; design: docs/design/otp-exit-signals.md). This is a model of the    *)
(* proposed v0.8 contract, not of the v0.7 code: the v0.7 behaviours are   *)
(* negative controls (Fault values below) that must fail.                  *)
(*                                                                         *)
(* Phases of an actor (design "Phases of an exit"):                        *)
(*   "run"      dispatching;                                               *)
(*   "stopping" exit begun: no dispatch; stopped() may run and send; an    *)
(*              untrappable kill still replaces the requested reason;      *)
(*   "gone"     past the death point D: the reason is final, the mailbox   *)
(*              refuses admission, every queued request was answered, the  *)
(*              link and monitor table is closed and one notice per edge   *)
(*              was taken; destruction (state drop) is in progress;        *)
(*   "dead"     published: wait_exit/join return; the notices taken at D   *)
(*              may now be delivered.                                      *)
(* Registration (link, monitor) and request admission are atomic with D:   *)
(* before it the edge fires (or the request is answered) with the real     *)
(* reason; after it they get NoProc.                                       *)
(*                                                                         *)
(* Exit signals (exit_signal/2): Kill is untrappable and the target ends   *)
(* Killed; any other signal to a trapping actor is queued as an Exit       *)
(* message, except that a signal from the actor's parent (its supervisor   *)
(* or start_linked parent) is queued as a parent exit that runs normal     *)
(* termination when dispatched; a Normal signal is ignored by a            *)
(* non-trapping actor; any other reason begins exit with that reason.      *)
(* Administrative stops (ChildHandle::stop and friends) are untrappable.   *)
(* Handlers are atomic steps; "after the current handler" is the next      *)
(* step. Liveness assumes handlers, stopped() and destruction return       *)
(* (weak fairness on Dispatch, Die and Publish).                           *)
(***************************************************************************)
CONSTANTS
    Actors,        \* actor names
    Children,      \* actors supervised by Sup (subset of Actors)
    LinkParent,    \* [Actors -> Actors \cup {None}]: start_linked parent
    Reqs,          \* request ids, each sent at most once by an outside caller
    MonIds,        \* monitor ids, each used at most once
    MaxSends,      \* bound on actor-to-actor messages
    MaxSignals,    \* bound on exit signals and administrative stops
    MaxLate,       \* bound on registrations against gone actors
    UnlinkEnabled, \* include Unlink
    TrapCanClear,  \* trap_exit may also be turned off
    SelfReasons,   \* reasons of self-exits (subset of Reasons)
    SignalReasons, \* reasons of exit signals (Kill is the kill signal)
    AdminReasons,  \* reasons of administrative stops
    DropEnabled,   \* the runtime may drop actors
    Fault          \* "None" or a negative control, see below

None == "none"
Ext == "ext"       \* outside code (a ChildHandle holder, a caller)
Sup == "sup"       \* the supervisor of Children
Reasons == {"Normal", "Shutdown", "Panic", "Kill", "Killed", "NoProc"}
Done == {"ok", "stopped", "noproc"}
Past(a, s) == s \in {"gone", "dead"}

(* Negative controls (each must produce a counterexample):                 *)
(*  DirectStop      v0.7: exit signals ignore trap_exit, Normal stops too  *)
(*  KillAsKill      v0.7: an outside kill ends Kill                        *)
(*  KillNoSupersede kill during stopping keeps the requested reason        *)
(*  LateRealReason  v0.7: link/monitor to a gone actor gets its reason     *)
(*  ParentAsExit    parent Shutdown reaches exit_received, may be ignored  *)
(*  Retain182       PR #182: a request published after D is kept           *)
(*  Dispatch185     PR #185: work dispatched after stop was observed       *)
(*  DropQueued      D does not answer queued requests                      *)
(*  EarlyNotify     links and monitors fire when exit begins               *)
(*  SplitDeathPoint registration checks publication instead of D           *)
(*  NoUnlinkFence   unlink does not cancel a link notice in flight         *)
(*  NoDemonitorFence demonitor does not cancel a Down in flight           *)
(*  EarlyStopped    a panicking request is answered before D               *)
(*  PanicOverKill   a panic during exit replaces Killed                    *)
(*  DropKeepsNormal a runtime drop keeps a requested Normal                *)
KilledReason == IF Fault = "KillAsKill" THEN "Kill" ELSE "Killed"

VARIABLES
    st,        \* [Actors -> {"run", "stopping", "gone", "dead"}]
    trap,      \* [Actors -> BOOLEAN]
    why,       \* [Actors -> Reasons \cup {None}]: requested, final from D
    cause,     \* [Actors -> causes]: what decided the reason (ghost)
    trapAt,    \* [Actors -> BOOLEAN]: trap flag when the exit began (ghost)
    mbox,      \* [Actors -> Seq(Item)]
    links,     \* set of two-element sets of actors
    mon,       \* [MonIds -> [o, t]]: owner and target
    mstate,    \* [MonIds -> {"unused", "active", "fired", "cancelled", "late"}]
    downs,     \* [MonIds -> Nat]: Down messages enqueued (ghost)
    inflight,  \* notices taken at D (or late registrations), not yet delivered
    fired,     \* actors whose links and monitors already fired (ghost)
    cancelled, \* notices cancelled by unlink/demonitor (ghost)
    afterCancel, \* a cancelled notice took effect (ghost)
    req,       \* [Reqs -> [t, s, why]]
    dropFrom,  \* [Actors -> requested reason when the runtime dropped it] (ghost)
    killed,    \* [Actors -> BOOLEAN]: an effective kill arrived before D (ghost)
    sends, signals, lates,
    psent,     \* children that received the supervisor's Shutdown
    afterStop  \* a message was dispatched after stop was observed (ghost)

vars == <<st, trap, why, cause, trapAt, mbox, links, mon, mstate, downs,
          inflight, fired, cancelled, afterCancel, req, dropFrom, killed, sends, signals,
          lates, psent, afterStop>>
life == <<st, why, cause, trapAt, dropFrom, killed>>
reg == <<links, mon, mstate, downs, inflight, fired, cancelled, afterCancel>>
budget == <<sends, signals, lates, psent>>

\* Mailbox items. o: "death" (link or monitor notice taken at D), "late"
\* (registration after D), "direct" (an exit signal sent by a live sender),
\* "none" (ordinary message or request).
Item(k, s, r, m, q, o) == [k |-> k, s |-> s, r |-> r, m |-> m, q |-> q, o |-> o]
\* g tells registrations apart: 0 for death notices (one per edge and death),
\* a fresh number for each late registration.
Note(to, from, k, r, m, o, g) ==
    [to |-> to, from |-> from, k |-> k, r |-> r, m |-> m, o |-> o, g |-> g]
IsParent(a, s) == s = LinkParent[a] \/ (s = Sup /\ a \in Children)
\* Registration and admission see D; the SplitDeathPoint control sees
\* publication instead.
IsGone(a) == IF Fault = "SplitDeathPoint" THEN st[a] = "dead" ELSE Past(a, st[a])
Push(a, x) == mbox' = [mbox EXCEPT ![a] = Append(@, x)]

Init ==
    /\ st = [a \in Actors |-> "run"]
    /\ trap = [a \in Actors |-> FALSE]
    /\ why = [a \in Actors |-> None]
    /\ cause = [a \in Actors |-> None]
    /\ trapAt = [a \in Actors |-> FALSE]
    /\ mbox = [a \in Actors |-> <<>>]
    /\ links = {{a, LinkParent[a]} : a \in {x \in Actors : LinkParent[x] # None}}
    /\ mon = [m \in MonIds |-> [o |-> None, t |-> None]]
    /\ mstate = [m \in MonIds |-> "unused"]
    /\ downs = [m \in MonIds |-> 0]
    /\ inflight = {}
    /\ fired = {}
    /\ cancelled = {}
    /\ afterCancel = FALSE
    /\ req = [r \in Reqs |-> [t |-> None, s |-> "idle", why |-> None]]
    /\ dropFrom = [a \in Actors |-> "nodrop"]
    /\ killed = [a \in Actors |-> FALSE]
    /\ sends = 0 /\ signals = 0 /\ lates = 0 /\ psent = {}
    /\ afterStop = FALSE

(* Exit begins once; the first requested reason wins until D, except kill. *)
Begin(a, r, c) ==
    IF st[a] = "run"
    THEN /\ st' = [st EXCEPT ![a] = "stopping"]
         /\ why' = [why EXCEPT ![a] = r]
         /\ cause' = [cause EXCEPT ![a] = c]
         /\ trapAt' = [trapAt EXCEPT ![a] = trap[a]]
         /\ UNCHANGED <<dropFrom, killed>>
    ELSE UNCHANGED life

(* An untrappable kill: begins exit, or replaces the requested reason of an *)
(* actor that is still before D (OTP kills a process inside terminate).    *)
Kill(a) ==
    IF st[a] = "run"
    THEN /\ st' = [st EXCEPT ![a] = "stopping"]
         /\ why' = [why EXCEPT ![a] = KilledReason]
         /\ cause' = [cause EXCEPT ![a] = "kill"]
         /\ trapAt' = [trapAt EXCEPT ![a] = trap[a]]
         /\ killed' = [killed EXCEPT ![a] = TRUE]
         /\ UNCHANGED dropFrom
    ELSE IF st[a] = "stopping"
    THEN /\ why' = IF Fault = "KillNoSupersede" THEN why ELSE [why EXCEPT ![a] = KilledReason]
         /\ cause' = [cause EXCEPT ![a] = "kill"]
         /\ killed' = [killed EXCEPT ![a] = TRUE]
         /\ UNCHANGED <<st, trapAt, dropFrom>>
    ELSE UNCHANGED life

(* An exit signal from s with reason r and origin o arrives at a. c is the *)
(* cause recorded if it ends a non-trapping actor.                         *)
Signal(a, s, r, o, c) ==
    IF st[a] # "run" THEN UNCHANGED <<life, mbox>>
    ELSE IF trap[a]
    THEN /\ Push(a, Item(IF IsParent(a, s) /\ Fault # "ParentAsExit" THEN "pexit" ELSE "exit",
                         s, r, 0, 0, o))
         /\ UNCHANGED life
    ELSE IF r = "Normal" THEN UNCHANGED <<life, mbox>>
    ELSE /\ Begin(a, r, c)
         /\ UNCHANGED mbox

(* The table of a closes: one notice per edge with the final reason r. *)
Fire(a, r) ==
    LET peers == {b \in Actors : {a, b} \in links}
        watched == {m \in MonIds : mstate[m] = "active" /\ mon[m].t = a}
        owned == {m \in MonIds : mstate[m] = "active" /\ mon[m].o = a}
    IN /\ inflight' = inflight
            \cup {Note(b, a, "link", r, 0, "death", 0) : b \in peers}
            \cup {Note(mon[m].o, a, "down", r, m, "death", 0) : m \in watched}
       /\ links' = {l \in links : a \notin l}
       /\ mstate' = [m \in MonIds |-> IF m \in watched THEN "fired"
                                      ELSE IF m \in owned THEN "cancelled"
                                      ELSE mstate[m]]
       /\ fired' = fired \cup {a}

(* The death point D of a with final reason r: admission closes, queued    *)
(* requests are answered, the table closes. Shared by Die and RuntimeDrop. *)
Death(a, r) ==
    /\ st' = [st EXCEPT ![a] = "gone"]
    /\ mbox' = [mbox EXCEPT ![a] = <<>>]
    /\ req' = [q \in Reqs |->
                 IF req[q].s \in {"queued", "exiting"} /\ req[q].t = a /\ Fault # "DropQueued"
                 THEN [req[q] EXCEPT !.s = "stopped", !.why = r] ELSE req[q]]
    /\ IF a \in fired THEN UNCHANGED <<inflight, links, mstate, fired>> ELSE Fire(a, r)

----------------------------------------------------------------------------
(* Turns of a running actor. *)

SetTrap(a, b) ==
    /\ st[a] = "run" /\ trap[a] # b /\ (b \/ TrapCanClear)
    /\ trap' = [trap EXCEPT ![a] = b]
    /\ UNCHANGED <<life, mbox, reg, req, budget, afterStop>>

Send(a, b) ==   \* also from stopped(): st[a] = "stopping"
    /\ st[a] \in {"run", "stopping"} /\ a # b /\ st[b] \in {"run", "stopping"}
    /\ sends < MaxSends /\ sends' = sends + 1
    /\ Push(b, Item("msg", a, None, 0, 0, "none"))
    /\ UNCHANGED <<life, trap, reg, req, signals, lates, psent, afterStop>>

SelfExit(a, r) ==   \* ctx.stop, ctx.stop_with_reason(r), a panic in any turn
    /\ st[a] = "run" /\ r \in SelfReasons
    /\ Begin(a, r, "self")
    /\ UNCHANGED <<trap, mbox, reg, req, budget, afterStop>>

(* A panic in user code while exiting (the current handler or stopped()):  *)
(* it replaces the requested reason, except Killed (OTP: a crash in        *)
(* terminate replaces the reason; a kill ends the process anyway).         *)
PanicWhileExiting(a) ==
    /\ st[a] = "stopping" /\ why[a] # "Panic"
    /\ ~killed[a] \/ Fault = "PanicOverKill"
    /\ why' = [why EXCEPT ![a] = "Panic"]
    /\ cause' = [cause EXCEPT ![a] = "self"]
    /\ UNCHANGED <<st, trapAt, dropFrom, killed, trap, mbox, reg, req, budget, afterStop>>

Link(a, b) ==   \* idempotent while a link or a pending link notice exists
    /\ st[a] = "run" /\ a # b /\ {a, b} \notin links
    /\ ~\E n \in inflight : n.to = a /\ n.from = b /\ n.k = "link"
    /\ IF IsGone(b)
       THEN /\ lates < MaxLate /\ lates' = lates + 1
            /\ IF trap[a]
               THEN /\ inflight' = inflight \cup
                        {Note(a, b, "link", IF Fault = "LateRealReason" THEN why[b] ELSE "NoProc", 0, "late", lates')}
                    /\ UNCHANGED <<life, links>>
               \* Not trapping: the design leaves open whether link returns an
               \* error (OTP's local noproc exception) or the caller exits
               \* NoProc (OTP's signal semantics). Both are allowed here.
               ELSE /\ \/ UNCHANGED life
                       \/ Begin(a, IF Fault = "LateRealReason" THEN why[b] ELSE "NoProc", "noproc")
                    /\ UNCHANGED <<inflight, links>>
       ELSE /\ links' = links \cup {{a, b}}
            /\ UNCHANGED <<life, inflight, lates>>
    /\ UNCHANGED <<trap, mbox, mon, mstate, downs, fired, cancelled, afterCancel,
                   req, sends, signals, psent, afterStop>>

(* Unlink: removes the link, and cancels a notice from b already taken at   *)
(* b's death but not yet delivered (OTP: once unlink returns, the link has  *)
(* no further effect), including a late NoProc notice (OTP keeps the link  *)
(* record while that response is pending). A queued Exit stays.            *)
Unlink(a, b) ==
    LET pending == {n \in inflight : n.to = a /\ n.from = b /\ n.k = "link"}
    IN /\ UnlinkEnabled /\ st[a] = "run"
       /\ ({a, b} \in links \/ pending # {})
       /\ links' = links \ {{a, b}}
       /\ cancelled' = cancelled \cup pending
       /\ inflight' = IF Fault = "NoUnlinkFence" THEN inflight ELSE inflight \ pending
       /\ UNCHANGED <<life, trap, mbox, mon, mstate, downs, fired, afterCancel,
                      req, budget, afterStop>>

Monitor(a, b, m) ==
    /\ st[a] = "run" /\ a # b /\ mstate[m] = "unused"
    /\ mon' = [mon EXCEPT ![m] = [o |-> a, t |-> b]]
    /\ IF IsGone(b)
       THEN /\ lates < MaxLate /\ lates' = lates + 1
            /\ mstate' = [mstate EXCEPT ![m] = "late"]
            /\ inflight' = inflight \cup
                 {Note(a, b, "down", IF Fault = "LateRealReason" THEN why[b] ELSE "NoProc", m, "late", lates')}
       ELSE /\ mstate' = [mstate EXCEPT ![m] = "active"]
            /\ UNCHANGED <<inflight, lates>>
    /\ UNCHANGED <<life, trap, mbox, links, downs, fired, cancelled, afterCancel,
                   req, sends, signals, psent, afterStop>>

(* Demonitor: no Down is enqueued after it returns; one already queued stays. *)
Demonitor(a, m) ==
    LET pending == {n \in inflight : n.k = "down" /\ n.m = m}
    IN /\ st[a] = "run" /\ mon[m].o = a
       /\ mstate[m] \in {"active", "fired", "late"} /\ (mstate[m] = "active" \/ pending # {})
       /\ mstate' = [mstate EXCEPT ![m] = "cancelled"]
       /\ cancelled' = cancelled \cup pending
       /\ inflight' = IF Fault = "NoDemonitorFence" THEN inflight ELSE inflight \ pending
       /\ UNCHANGED <<life, trap, mbox, links, mon, downs, fired, afterCancel,
                      req, budget, afterStop>>

(* One message is dispatched. Handlers are atomic and nondeterministic. *)
Dispatch(a) ==
    /\ st[a] = "run" \/ (Fault = "Dispatch185" /\ st[a] = "stopping")
    /\ mbox[a] # <<>>
    /\ afterStop' = (afterStop \/ st[a] # "run")
    /\ LET h == Head(mbox[a]) IN
       /\ mbox' = [mbox EXCEPT ![a] = Tail(@)]
       /\ CASE h.k = "req" ->
                 \/ /\ req' = [req EXCEPT ![h.q].s = "ok"]
                    /\ UNCHANGED life
                 \/ /\ req' = [req EXCEPT ![h.q].s =                    \* handler panic:
                              IF Fault = "EarlyStopped" THEN "stopped" ELSE "exiting",
                                          ![h.q].why = IF Fault = "EarlyStopped" THEN "Panic" ELSE None]
                    /\ Begin(a, "Panic", "self")
            [] h.k = "exit" ->   \* exit_received: ignore, or stop itself
                 /\ \/ UNCHANGED life
                    \/ \E r \in {"Normal", h.r} : Begin(a, r, "self")
                 /\ UNCHANGED req
            [] h.k = "pexit" ->  \* parent exit: normal termination (gen_server)
                 /\ Begin(a, h.r, "parent")
                 /\ UNCHANGED req
            [] OTHER ->          \* msg or down (a panic here is SelfExit)
                 UNCHANGED <<life, req>>
    /\ UNCHANGED <<trap, reg, budget>>

(* stopped() returned: the death point D. *)
Die(a) ==
    /\ st[a] = "stopping"
    /\ Death(a, why[a])
    /\ UNCHANGED <<why, cause, trapAt, dropFrom, killed, trap, mon, downs, cancelled, afterCancel,
                   budget, afterStop>>

(* The runtime drops the actor (tasks: the future is dropped; threads: the  *)
(* closure never ran). No stopped(); reason Killed unless a non-Normal one  *)
(* was already requested (G1).                                             *)
RuntimeDrop(a) ==
    /\ DropEnabled /\ st[a] \in {"run", "stopping"}
    /\ LET r == IF why[a] = None \/ (why[a] = "Normal" /\ Fault # "DropKeepsNormal")
                THEN "Killed" ELSE why[a] IN
       /\ why' = [why EXCEPT ![a] = r]
       /\ dropFrom' = [dropFrom EXCEPT ![a] = why[a]]
       /\ cause' = [cause EXCEPT ![a] = IF why[a] \in {None, "Normal"} THEN "drop" ELSE cause[a]]
       /\ trapAt' = [trapAt EXCEPT ![a] = IF st[a] = "run" THEN trap[a] ELSE trapAt[a]]
       /\ Death(a, r)
    /\ UNCHANGED <<killed, trap, mon, downs, cancelled, afterCancel, budget, afterStop>>

EarlyFire(a) ==   \* Fault EarlyNotify only
    /\ Fault = "EarlyNotify" /\ st[a] = "stopping" /\ a \notin fired
    /\ Fire(a, why[a])
    /\ UNCHANGED <<life, trap, mbox, mon, downs, cancelled, afterCancel, req, budget, afterStop>>

(* Destruction finished: the exit is published. *)
Publish(a) ==
    /\ st[a] = "gone"
    /\ st' = [st EXCEPT ![a] = "dead"]
    /\ UNCHANGED <<why, cause, trapAt, dropFrom, killed, trap, mbox, reg, req, budget, afterStop>>

(* A notice taken at D is delivered after publication; late notices at once. *)
Deliver(n) ==
    /\ n \in inflight
    /\ n.o = "late" \/ st[n.from] = "dead" \/ Fault = "EarlyNotify"
    /\ inflight' = inflight \ {n}
    /\ afterCancel' = (afterCancel \/ n \in cancelled)
    /\ IF n.k = "down"
       THEN IF st[n.to] \in {"gone", "dead"}
            THEN UNCHANGED <<life, mbox, downs>>
            ELSE /\ Push(n.to, Item("down", n.from, n.r, n.m, 0, n.o))
                 /\ downs' = [downs EXCEPT ![n.m] = @ + 1]
                 /\ UNCHANGED life
       ELSE /\ Signal(n.to, n.from, n.r, n.o, "link")
            /\ UNCHANGED downs
    /\ UNCHANGED <<trap, links, mon, mstate, fired, cancelled, req, budget, afterStop>>

----------------------------------------------------------------------------
(* Outside world: callers, ChildHandle holders, the supervisor. *)

Request(r, t) ==   \* the caller begins a request; enqueue is a separate step
    /\ req[r].s = "idle"
    /\ req' = [req EXCEPT ![r] = [t |-> t, s |-> "sending", why |-> None]]
    /\ UNCHANGED <<life, trap, mbox, reg, budget, afterStop>>

Enqueue(r) ==
    /\ req[r].s = "sending"
    /\ LET t == req[r].t IN
       IF Past(t, st[t]) /\ Fault # "Retain182"
       THEN /\ req' = [req EXCEPT ![r].s = "noproc"]   \* not admitted
            /\ UNCHANGED mbox
       ELSE /\ req' = [req EXCEPT ![r].s = "queued"]
            /\ Push(t, Item("req", Ext, None, 0, r, "none"))
    /\ UNCHANGED <<life, trap, reg, budget, afterStop>>

(* exit_signal(a, r) from s: ctx.exit(&a, r) from actor s, or            *)
(* ChildHandle::exit(r) from outside (s = Ext). r = Kill is kill().       *)
ExitSignal(s, a, r) ==
    /\ r \in SignalReasons /\ signals < MaxSignals /\ signals' = signals + 1
    /\ IF s = Ext THEN TRUE ELSE st[s] \in {"run", "stopping"}   \* inert after D
    /\ IF r = "Kill"
       THEN /\ Kill(a) /\ UNCHANGED mbox
       ELSE IF Fault = "DirectStop"
       THEN /\ Begin(a, r, "signal") /\ UNCHANGED mbox
       ELSE Signal(a, s, r, "direct", "signal")
    /\ UNCHANGED <<trap, reg, req, sends, lates, psent, afterStop>>

AdminStop(a, r) ==    \* ChildHandle::stop / stop_with_reason / shutdown
    /\ r \in AdminReasons /\ signals < MaxSignals /\ signals' = signals + 1
    /\ Begin(a, r, "admin")
    /\ UNCHANGED <<trap, mbox, reg, req, sends, lates, psent, afterStop>>

SupShutdown(c) ==     \* exit(Child, shutdown) from the parent, also to a child
    /\ c \in Children /\ c \notin psent /\ st[c] \in {"run", "stopping"}   \* already exiting
    /\ psent' = psent \cup {c}
    /\ Signal(c, Sup, "Shutdown", "direct", "parent")
    /\ UNCHANGED <<trap, reg, req, sends, signals, lates, afterStop>>

SupKill(c) ==         \* shutdown_timeout passed: exit(Child, kill)
    /\ c \in psent /\ st[c] \in {"run", "stopping"}
    /\ Kill(c)
    /\ UNCHANGED <<trap, mbox, reg, req, budget, afterStop>>

Next ==
    \/ \E a \in Actors :
         \/ \E b \in BOOLEAN : SetTrap(a, b)
         \/ \E b \in Actors : Send(a, b) \/ Link(a, b) \/ Unlink(a, b)
         \/ \E b \in Actors, m \in MonIds : Monitor(a, b, m)
         \/ \E m \in MonIds : Demonitor(a, m)
         \/ PanicWhileExiting(a)
         \/ \E r \in Reasons : SelfExit(a, r) \/ AdminStop(a, r)
                               \/ \E s \in Actors \cup {Ext} : ExitSignal(s, a, r)
         \/ Dispatch(a) \/ Die(a) \/ RuntimeDrop(a) \/ EarlyFire(a) \/ Publish(a)
         \/ SupShutdown(a) \/ SupKill(a)
    \/ \E n \in inflight : Deliver(n)
    \/ \E r \in Reqs : Enqueue(r) \/ \E t \in Actors : Request(r, t)

Fairness ==
    /\ \A a \in Actors : WF_vars(Dispatch(a)) /\ WF_vars(Die(a))
                         /\ WF_vars(EarlyFire(a)) /\ WF_vars(Publish(a))
    /\ WF_vars(\E n \in inflight : Deliver(n))
    /\ \A r \in Reqs : WF_vars(Enqueue(r))

Spec == Init /\ [][Next]_vars
FairSpec == Spec /\ Fairness

----------------------------------------------------------------------------
(* Safety *)

Causes == {None, "self", "kill", "link", "parent", "signal", "admin", "noproc", "drop"}
TypeOK ==
    /\ st \in [Actors -> {"run", "stopping", "gone", "dead"}]
    /\ trap \in [Actors -> BOOLEAN]
    /\ why \in [Actors -> Reasons \cup {None}]
    /\ cause \in [Actors -> Causes]
    /\ mstate \in [MonIds -> {"unused", "active", "fired", "cancelled", "late"}]
    /\ \A r \in Reqs : req[r].s \in {"idle", "sending", "queued", "exiting"} \cup Done
    /\ \A a \in Actors : (st[a] = "run") <=> (why[a] = None)

\* A queued request is in exactly one live mailbox; past D it was answered
\* (PR #182; rank 11 death boundary).
NoOrphanRequest ==
    \A r \in Reqs : req[r].s = "queued" =>
        /\ st[req[r].t] \in {"run", "stopping"}
        /\ Len(SelectSeq(mbox[req[r].t], LAMBDA x : x.k = "req" /\ x.q = r)) = 1

\* An exit error reaches a caller only at D, with the final reason; until
\* then a request whose handler panicked is held (rank 11 slot) open.
StoppedAtDeath ==
    \A r \in Reqs :
        /\ req[r].s = "exiting" => st[req[r].t] = "stopping"
        /\ req[r].s = "stopped" => Past(req[r].t, st[req[r].t]) /\ req[r].why = why[req[r].t]

\* No message is dispatched once the actor observed its stop (PR #185).
NoDispatchAfterStop == ~afterStop

\* At most one Down per monitor.
DownAtMostOnce == \A m \in MonIds : downs[m] <= 1

\* NoProc exactly for registrations after D; a death notice carries the
\* source's final reason; NoProc as a caller's own reason comes from link.
Queued(a) ==
    {Note(a, mbox[a][i].s, mbox[a][i].k, mbox[a][i].r, mbox[a][i].m, mbox[a][i].o, 0) :
        i \in {j \in 1..Len(mbox[a]) : mbox[a][j].o \in {"death", "late"}}}
Notices == inflight \cup UNION {Queued(a) : a \in Actors}
NoProcIffLate ==
    /\ \A n \in Notices :
        /\ n.o = "late" => n.r = "NoProc"
        /\ n.o = "death" => n.r = why[n.from]
    /\ \A a \in Actors : cause[a] = "noproc" => why[a] = "NoProc"

\* Only a kill (or a runtime drop) bypasses trap_exit; parent exits and
\* administrative stops end a trapping actor through normal termination.
TrapRespected ==
    \A a \in Actors : st[a] # "run" /\ trapAt[a] =>
        cause[a] \in {"self", "kill", "parent", "admin", "drop"}

\* A Normal exit signal or link notice never stops a non-trapping actor.
NormalSignalIgnored ==
    \A a \in Actors : cause[a] \in {"link", "signal"} => why[a] # "Normal"

\* An effective kill (before D) ends the target Killed, also when it
\* arrives during exit, and nothing replaces it (kill provenance, recorded
\* in a ghost no other action changes).
KilledFromKill == \A a \in Actors : killed[a] => why[a] = "Killed"

\* A runtime drop ends Killed unless a non-Normal reason was requested (G1).
RuntimeDropReason ==
    \A a \in Actors : dropFrom[a] # "nodrop" =>
        why[a] = IF dropFrom[a] \in {None, "Normal"} THEN "Killed" ELSE dropFrom[a]

\* Registration is atomic with D: no live edge to an actor past D.
NoEdgeToDead ==
    /\ \A m \in MonIds : mstate[m] = "active" => st[mon[m].t] \in {"run", "stopping"}
    /\ \A l \in links : \A x \in l : st[x] \in {"run", "stopping"}

\* A death notice follows every message its source sent earlier to the same
\* observer, including direct exit signals it sent before D.
ExitAfterMessages ==
    \A a \in Actors : \A i, j \in 1..Len(mbox[a]) :
        (i < j /\ mbox[a][i].o = "death" /\ (mbox[a][j].k = "msg" \/ mbox[a][j].o = "direct"))
            => mbox[a][j].s # mbox[a][i].s

\* Nothing takes effect after unlink or demonitor cancelled it.
CancelFenced == ~afterCancel

Safety ==
    /\ TypeOK /\ NoOrphanRequest /\ NoDispatchAfterStop /\ DownAtMostOnce
    /\ NoProcIffLate /\ TrapRespected /\ NormalSignalIgnored /\ KilledFromKill
    /\ NoEdgeToDead /\ ExitAfterMessages /\ CancelFenced /\ StoppedAtDeath
    /\ RuntimeDropReason

----------------------------------------------------------------------------
(* Liveness (under FairSpec: handlers, stopped() and destruction return) *)

\* Every request that was sent ends Ok, ActorStopped or NoProc.
RequestsAnswered == \A r \in Reqs : (req[r].s \in {"sending", "queued", "exiting"}) ~> (req[r].s \in Done)
\* Exit, once begun, is published.
ExitCompletes == \A a \in Actors : (st[a] = "stopping") ~> (st[a] = "dead")
\* A supervisor Shutdown ends the child, trapping or not, without the kill.
ShutdownEndsChild == \A c \in Children : (c \in psent) ~> (st[c] = "dead")
\* Down exactly once: a monitor whose target is dead (fired at D, late, or
\* still registered) delivers its Down unless the observer is gone or
\* demonitored it.
DownDelivered ==
    \A m \in MonIds :
        (\/ mstate[m] \in {"fired", "late"}
         \/ (mstate[m] = "active" /\ mon[m].t \in Actors /\ st[mon[m].t] = "dead")) ~>
        (downs[m] = 1 \/ mstate[m] = "cancelled"
            \/ (mon[m].o \in Actors /\ st[mon[m].o] \in {"gone", "dead"}))
\* An abnormal death notice reaches its linked peer: a non-trapping peer
\* begins exit, a trapping one gets an Exit or parent-exit message, unless
\* the peer unlinked first or was already exiting.
LinkedPeersStop ==
    \A a, b \in Actors : \A r \in Reasons \ {"Normal"} :
        (Note(b, a, "link", r, 0, "death", 0) \in inflight) ~>
            \/ st[b] # "run"
            \/ Note(b, a, "link", r, 0, "death", 0) \in cancelled
            \/ \E i \in 1..Len(mbox[b]) :
                   /\ mbox[b][i].k \in {"exit", "pexit"} /\ mbox[b][i].s = a
                   /\ mbox[b][i].o = "death" /\ mbox[b][i].r = r
=============================================================================
