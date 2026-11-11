----------------------------- MODULE Lifecycle -----------------------------
EXTENDS Naturals, Sequences, FiniteSets
(***************************************************************************)
(* Base actor lifecycle with exit signals as in OTP (ROADMAP ranks 5e and  *)
(* 5g; design: docs/design/otp-exit-signals.md). This is a model of the    *)
(* proposed v0.8 contract, not of the v0.7 code: the v0.7 behaviours are   *)
(* negative controls (Fault values below) that must fail.                  *)
(*                                                                         *)
(* An actor is "run" (dispatching), "stopping" (exit begun: no dispatch,   *)
(* stopped() may still run and send) or "dead". Death is one atomic step   *)
(* (design rule D): the mailbox is disposed and every queued request is    *)
(* answered ActorStopped, then the link and monitor table closes and one   *)
(* notification per link and monitor is put in flight. Registration        *)
(* (link, monitor) is atomic with that close: before it, the edge fires at *)
(* death with the real reason; after it, a NoProc notice is produced.      *)
(*                                                                         *)
(* Exit signals: Kill from outside is untrappable and the target exits     *)
(* Killed; any other signal to a trapping actor is queued as an Exit       *)
(* message, except that an exit signal from the actor's parent (the        *)
(* supervisor, or the start_linked parent) is queued as a parent exit that *)
(* runs normal termination when dispatched; a Normal signal is ignored by  *)
(* a non-trapping actor; any other reason begins exit with that reason.    *)
(* Administrative stop (ChildHandle::stop / stop_with_reason) stays an     *)
(* untrappable cooperative stop. Handlers are atomic steps; "after the     *)
(* current handler" is the next step.                                      *)
(***************************************************************************)
CONSTANTS
    Actors,        \* actor names
    Children,      \* actors supervised by Sup (subset of Actors)
    LinkParent,    \* [Actors -> Actors \cup {None}]: start_linked parent
    Reqs,          \* request ids, each sent at most once by an outside caller
    MonIds,        \* monitor ids, each used at most once
    MaxSends,      \* bound on actor-to-actor messages
    MaxSignals,    \* bound on exit signals and administrative stops from outside
    MaxLate,       \* bound on registrations against dead actors
    UnlinkEnabled, \* include Unlink (deep configuration only)
    Fault          \* "None" or a negative control, see below

None == "none"
Ext == "ext"       \* an outside ChildHandle holder
Sup == "sup"       \* the supervisor of Children
Reasons == {"Normal", "Shutdown", "Panic", "Kill", "Killed", "NoProc"}
Done == {"ok", "stopped", "noproc"}

(* Negative controls (each must produce a counterexample):                 *)
(*  DirectStop      v0.7: exit signals ignore trap_exit, Normal stops too  *)
(*  KillAsKill      v0.7: an outside kill exits Kill, peers receive Kill   *)
(*  LateRealReason  v0.7: link/monitor to a dead actor gets its reason     *)
(*  ParentAsExit    parent Shutdown reaches exit_received, may be ignored  *)
(*  Retain182       PR #182: a request published after death is kept       *)
(*  Dispatch185     PR #185: work dispatched after stop was observed       *)
(*  DropQueued      death does not answer queued requests                  *)
(*  EarlyNotify     links and monitors fire when exit begins, not at death *)
(*  SplitDeathPoint registration checks a later "published" flag           *)
KilledReason == IF Fault = "KillAsKill" THEN "Kill" ELSE "Killed"

VARIABLES
    st,        \* [Actors -> {"run", "stopping", "dead"}]
    trap,      \* [Actors -> BOOLEAN]
    why,       \* [Actors -> Reasons \cup {None}]: committed exit reason
    cause,     \* [Actors -> causes]: what began the exit (ghost)
    trapAt,    \* [Actors -> BOOLEAN]: trap flag when the exit began (ghost)
    mbox,      \* [Actors -> Seq(Item)]
    links,     \* set of two-element sets of actors
    mon,       \* [MonIds -> [o, t]]: owner and target
    mstate,    \* [MonIds -> {"unused", "active", "fired", "cancelled", "late"}]
    downs,     \* [MonIds -> Nat]: Down messages enqueued (ghost)
    inflight,  \* set of notices taken at death, not yet delivered
    fired,     \* actors whose links and monitors already fired (ghost)
    published, \* actors whose death is visible to registration
    req,       \* [Reqs -> [t, s]]
    sends, signals, lates,
    psent,     \* children that received the supervisor's Shutdown
    afterStop  \* a message was dispatched after stop was observed (ghost)

vars == <<st, trap, why, cause, trapAt, mbox, links, mon, mstate, downs,
          inflight, fired, published, req, sends, signals, lates, psent, afterStop>>
life == <<st, why, cause, trapAt>>
reg == <<links, mon, mstate, downs, inflight, fired, published>>
budget == <<sends, signals, lates, psent>>

Item(k, s, r, m, q, late) == [k |-> k, s |-> s, r |-> r, m |-> m, q |-> q, late |-> late]
Note(to, from, k, r, m, late) ==
    [to |-> to, from |-> from, k |-> k, r |-> r, m |-> m, late |-> late]
IsParent(a, s) == s = LinkParent[a] \/ (s = Sup /\ a \in Children)
IsDead(a) == IF Fault = "SplitDeathPoint" THEN a \in published ELSE st[a] = "dead"
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
    /\ published = {}
    /\ req = [r \in Reqs |-> [t |-> None, s |-> "idle"]]
    /\ sends = 0 /\ signals = 0 /\ lates = 0 /\ psent = {}
    /\ afterStop = FALSE

(* Exit begins once; the first committed reason wins. *)
Begin(a, r, c) ==
    IF st[a] = "run"
    THEN /\ st' = [st EXCEPT ![a] = "stopping"]
         /\ why' = [why EXCEPT ![a] = r]
         /\ cause' = [cause EXCEPT ![a] = c]
         /\ trapAt' = [trapAt EXCEPT ![a] = trap[a]]
    ELSE UNCHANGED life

(* An exit signal from s (an actor, Ext or Sup) with reason r arrives at a.  *)
(* Used for link notices and for direct signals; the caller updates mbox    *)
(* only through this operator.                                             *)
Signal(a, s, r, late, c) ==
    IF st[a] = "dead" THEN UNCHANGED <<st, why, cause, trapAt, mbox>>
    ELSE IF trap[a]
    THEN /\ Push(a, Item(IF IsParent(a, s) /\ Fault # "ParentAsExit" THEN "pexit" ELSE "exit",
                         s, r, 0, 0, late))
         /\ UNCHANGED life
    ELSE IF r = "Normal" THEN UNCHANGED <<st, why, cause, trapAt, mbox>>
    ELSE /\ Begin(a, r, c)
         /\ UNCHANGED mbox

(* Links and monitors of a fire: one notice per edge, real reason r. *)
Fire(a, r) ==
    LET peers == {b \in Actors : {a, b} \in links}
        watched == {m \in MonIds : mstate[m] = "active" /\ mon[m].t = a}
        owned == {m \in MonIds : mstate[m] = "active" /\ mon[m].o = a}
    IN /\ inflight' = inflight
            \cup {Note(b, a, "link", r, 0, FALSE) : b \in peers}
            \cup {Note(mon[m].o, a, "down", r, m, FALSE) : m \in watched}
       /\ links' = {l \in links : a \notin l}
       /\ mstate' = [m \in MonIds |-> IF m \in watched THEN "fired"
                                      ELSE IF m \in owned THEN "cancelled"
                                      ELSE mstate[m]]
       /\ fired' = fired \cup {a}

----------------------------------------------------------------------------
(* Turns of a running actor. *)

SetTrap(a) ==   \* ctx.trap_exit(true); turning it off adds no rule to check
    /\ st[a] = "run" /\ ~trap[a]
    /\ trap' = [trap EXCEPT ![a] = TRUE]
    /\ UNCHANGED <<life, mbox, reg, req, budget, afterStop>>

Send(a, b) ==   \* also from stopped(): st[a] = "stopping"
    /\ st[a] \in {"run", "stopping"} /\ a # b /\ st[b] # "dead"
    /\ sends < MaxSends /\ sends' = sends + 1
    /\ Push(b, Item("msg", a, None, 0, 0, FALSE))
    /\ UNCHANGED <<life, trap, reg, req, signals, lates, psent, afterStop>>

SelfExit(a, r) ==   \* ctx.stop(), ctx.stop_with_reason(Kill), a panic in any turn
    /\ st[a] = "run" /\ r \in {"Normal", "Panic", "Kill"}
    /\ Begin(a, r, "self")
    /\ UNCHANGED <<trap, mbox, reg, req, budget, afterStop>>

Link(a, b) ==
    /\ st[a] = "run" /\ a # b /\ {a, b} \notin links
    /\ IF IsDead(b)
       THEN /\ lates < MaxLate /\ lates' = lates + 1
            /\ IF trap[a]
               THEN /\ inflight' = inflight \cup
                        {Note(a, b, "link", IF Fault = "LateRealReason" THEN why[b] ELSE "NoProc", 0, TRUE)}
                    /\ UNCHANGED <<life, links>>
               \* Not trapping: the design leaves open whether link returns an
               \* error (OTP's noproc exception) or the caller exits NoProc
               \* (OTP's signal path). Both are allowed here.
               ELSE /\ \/ UNCHANGED life
                       \/ Begin(a, IF Fault = "LateRealReason" THEN why[b] ELSE "NoProc", "noproc")
                    /\ UNCHANGED <<inflight, links>>
       ELSE /\ links' = links \cup {{a, b}}
            /\ UNCHANGED <<life, inflight, lates>>
    /\ UNCHANGED <<trap, mbox, mon, mstate, downs, fired, published, req, sends, signals, psent, afterStop>>

Unlink(a, b) ==   \* does not retract a notice already in flight
    /\ st[a] = "run" /\ {a, b} \in links
    /\ links' = links \ {{a, b}}
    /\ UNCHANGED <<life, trap, mbox, mon, mstate, downs, inflight, fired, published, req, budget, afterStop>>

Monitor(a, b, m) ==
    /\ st[a] = "run" /\ a # b /\ mstate[m] = "unused"
    /\ mon' = [mon EXCEPT ![m] = [o |-> a, t |-> b]]
    /\ IF IsDead(b)
       THEN /\ lates < MaxLate /\ lates' = lates + 1
            /\ mstate' = [mstate EXCEPT ![m] = "late"]
            /\ inflight' = inflight \cup
                 {Note(a, b, "down", IF Fault = "LateRealReason" THEN why[b] ELSE "NoProc", m, TRUE)}
       ELSE /\ mstate' = [mstate EXCEPT ![m] = "active"]
            /\ UNCHANGED <<inflight, lates>>
    /\ UNCHANGED <<life, trap, mbox, links, downs, fired, published, req, sends, signals, psent, afterStop>>

Demonitor(a, m) ==   \* a Down already in flight or queued may still arrive
    /\ st[a] = "run" /\ mstate[m] = "active" /\ mon[m].o = a
    /\ mstate' = [mstate EXCEPT ![m] = "cancelled"]
    /\ UNCHANGED <<life, trap, mbox, links, mon, downs, inflight, fired, published, req, budget, afterStop>>

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
                 \/ /\ req' = [req EXCEPT ![h.q].s = "stopped"]   \* handler panic
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

(* stopped() returned: death. *)
Die(a) ==
    /\ st[a] = "stopping"
    /\ st' = [st EXCEPT ![a] = "dead"]
    /\ mbox' = [mbox EXCEPT ![a] = <<>>]
    /\ req' = [r \in Reqs |->
                 IF req[r].s = "queued" /\ req[r].t = a /\ Fault # "DropQueued"
                 THEN [req[r] EXCEPT !.s = "stopped"] ELSE req[r]]
    /\ IF a \in fired THEN UNCHANGED <<inflight, links, mstate, fired>> ELSE Fire(a, why[a])
    /\ published' = IF Fault = "SplitDeathPoint" THEN published ELSE published \cup {a}
    /\ UNCHANGED <<why, cause, trapAt, trap, mon, downs, budget, afterStop>>

EarlyFire(a) ==   \* Fault EarlyNotify only
    /\ Fault = "EarlyNotify" /\ st[a] = "stopping" /\ a \notin fired
    /\ Fire(a, why[a])
    /\ UNCHANGED <<life, trap, mbox, mon, downs, published, req, budget, afterStop>>

Publish(a) ==     \* Fault SplitDeathPoint only
    /\ Fault = "SplitDeathPoint" /\ st[a] = "dead" /\ a \notin published
    /\ published' = published \cup {a}
    /\ UNCHANGED <<life, trap, mbox, links, mon, mstate, downs, inflight, fired, req, budget, afterStop>>

Deliver(n) ==
    /\ n \in inflight
    /\ inflight' = inflight \ {n}
    /\ IF n.k = "down"
       THEN IF st[n.to] = "dead"
            THEN UNCHANGED <<life, mbox, downs>>
            ELSE /\ Push(n.to, Item("down", n.from, n.r, n.m, 0, n.late))
                 /\ downs' = [downs EXCEPT ![n.m] = @ + 1]
                 /\ UNCHANGED life
       ELSE /\ Signal(n.to, n.from, n.r, n.late, "link")
            /\ UNCHANGED downs
    /\ UNCHANGED <<trap, links, mon, mstate, fired, published, req, budget, afterStop>>

----------------------------------------------------------------------------
(* Outside world: callers, ChildHandle holders, the supervisor. *)

Request(r, t) ==   \* the caller begins a request; enqueue is a separate step
    /\ req[r].s = "idle"
    /\ req' = [req EXCEPT ![r] = [t |-> t, s |-> "sending"]]
    /\ UNCHANGED <<life, trap, mbox, reg, budget, afterStop>>

Enqueue(r) ==
    /\ req[r].s = "sending"
    /\ LET t == req[r].t IN
       IF st[t] = "dead" /\ Fault # "Retain182"
       THEN /\ req' = [req EXCEPT ![r].s = "noproc"]   \* not accepted
            /\ UNCHANGED mbox
       ELSE /\ req' = [req EXCEPT ![r].s = "queued"]
            /\ Push(t, Item("req", Ext, None, 0, r, FALSE))
    /\ UNCHANGED <<life, trap, reg, budget, afterStop>>

ExitSignal(a, r) ==   \* ChildHandle::exit(r); r = Kill is ChildHandle::kill()
    /\ signals < MaxSignals /\ signals' = signals + 1
    /\ r \in {"Normal", "Shutdown", "Kill"}
    /\ IF r = "Kill"
       THEN /\ Begin(a, KilledReason, "kill") /\ UNCHANGED mbox
       ELSE IF Fault = "DirectStop"
       THEN /\ Begin(a, r, "signal") /\ UNCHANGED mbox
       ELSE Signal(a, Ext, r, FALSE, "signal")
    /\ UNCHANGED <<trap, reg, req, sends, lates, psent, afterStop>>

AdminStop(a, r) ==    \* ChildHandle::stop / stop_with_reason: untrappable, cooperative
    /\ signals < MaxSignals /\ signals' = signals + 1
    /\ r = "Shutdown"
    /\ Begin(a, r, "admin")
    /\ UNCHANGED <<trap, mbox, reg, req, sends, lates, psent, afterStop>>

SupShutdown(c) ==     \* exit(Child, shutdown) from the parent
    /\ c \in Children /\ c \notin psent /\ st[c] # "dead"
    /\ psent' = psent \cup {c}
    /\ Signal(c, Sup, "Shutdown", FALSE, "parent")
    /\ UNCHANGED <<trap, reg, req, sends, signals, lates, afterStop>>

SupKill(c) ==         \* shutdown_timeout passed: exit(Child, kill)
    /\ c \in psent /\ st[c] = "run"
    /\ Begin(c, KilledReason, "kill")
    /\ UNCHANGED <<trap, mbox, reg, req, budget, afterStop>>

Next ==
    \/ \E a \in Actors :
         \/ SetTrap(a)
         \/ \E b \in Actors : Send(a, b) \/ Link(a, b) \/ (UnlinkEnabled /\ Unlink(a, b))
         \/ \E b \in Actors, m \in MonIds : Monitor(a, b, m)
         \/ \E m \in MonIds : Demonitor(a, m)
         \/ \E r \in Reasons : SelfExit(a, r) \/ ExitSignal(a, r) \/ AdminStop(a, r)
         \/ Dispatch(a) \/ Die(a) \/ EarlyFire(a) \/ Publish(a)
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

Causes == {None, "self", "kill", "link", "parent", "signal", "admin", "noproc"}
TypeOK ==
    /\ st \in [Actors -> {"run", "stopping", "dead"}]
    /\ trap \in [Actors -> BOOLEAN]
    /\ why \in [Actors -> Reasons \cup {None}]
    /\ cause \in [Actors -> Causes]
    /\ mstate \in [MonIds -> {"unused", "active", "fired", "cancelled", "late"}]
    /\ \A r \in Reqs : req[r].s \in {"idle", "sending", "queued"} \cup Done
    /\ \A a \in Actors : (st[a] = "run") <=> (why[a] = None)

\* Every request is answered at most once, and a queued request never sits
\* in a dead actor's mailbox (PR #182; rank 11 death boundary).
NoOrphanRequest ==
    \A r \in Reqs : req[r].s = "queued" =>
        /\ st[req[r].t] # "dead"
        /\ Len(SelectSeq(mbox[req[r].t], LAMBDA x : x.k = "req" /\ x.q = r)) = 1

\* No message is dispatched once the actor observed its stop (PR #185).
NoDispatchAfterStop == ~afterStop

\* At most one Down per monitor.
DownAtMostOnce == \A m \in MonIds : downs[m] <= 1

\* NoProc exactly for registrations against an actor that already died;
\* every other notice carries the source's committed reason.
Queued(a) ==
    {Note(a, mbox[a][i].s, mbox[a][i].k, mbox[a][i].r, mbox[a][i].m, mbox[a][i].late) :
        i \in {j \in 1..Len(mbox[a]) :
            mbox[a][j].k \in {"exit", "pexit", "down"} /\ mbox[a][j].s \in Actors}}
Notices == inflight \cup UNION {Queued(a) : a \in Actors}
NoProcIffLate ==
    \A n \in Notices :
        /\ n.late => n.r = "NoProc"
        /\ ~n.late => n.r = why[n.from]

\* Only an outside kill bypasses trap_exit; parent exits and administrative
\* stops end a trapping actor through its normal termination path.
TrapRespected ==
    \A a \in Actors : st[a] # "run" /\ trapAt[a] =>
        cause[a] \in {"self", "kill", "parent", "admin"}

\* A Normal exit signal never stops a non-trapping actor.
NormalSignalIgnored ==
    \A a \in Actors : cause[a] \in {"link", "signal"} => why[a] # "Normal"

\* An outside kill exits Killed, and only kills and their propagation produce
\* Killed: over a link, through a parent exit (a trapping child ends with its
\* parent's reason), or a trapping actor choosing to exit with it.
KilledFromKill ==
    \A a \in Actors :
        /\ cause[a] = "kill" => why[a] = "Killed"
        /\ why[a] = "Killed" => cause[a] \in {"kill", "link", "parent", "self"}

\* Registration is atomic with the death point: no live edge to a dead actor.
NoEdgeToDead ==
    /\ \A m \in MonIds : mstate[m] = "active" => st[mon[m].t] # "dead"
    /\ \A l \in links : \A x \in l : st[x] # "dead"

\* Exit and Down follow every message the source sent earlier to the same
\* observer (signal ordering between a pair of processes).
ExitAfterMessages ==
    \A a \in Actors : \A i, j \in 1..Len(mbox[a]) :
        (i < j /\ mbox[a][i].k \in {"exit", "pexit", "down"} /\ ~mbox[a][i].late
               /\ mbox[a][j].k = "msg")
            => mbox[a][j].s # mbox[a][i].s

Safety ==
    /\ TypeOK /\ NoOrphanRequest /\ NoDispatchAfterStop /\ DownAtMostOnce
    /\ NoProcIffLate /\ TrapRespected /\ NormalSignalIgnored /\ KilledFromKill
    /\ NoEdgeToDead /\ ExitAfterMessages

----------------------------------------------------------------------------
(* Liveness (under FairSpec) *)

\* Every request that was sent ends Ok, ActorStopped or NoProc.
RequestsAnswered == \A r \in Reqs : (req[r].s \in {"sending", "queued"}) ~> (req[r].s \in Done)
\* Exit, once begun, reaches death (stopped() returns; cleanup is cooperative).
ExitCompletes == \A a \in Actors : (st[a] = "stopping") ~> (st[a] = "dead")
\* A supervisor Shutdown ends the child, trapping or not, without a kill.
ShutdownEndsChild == \A c \in Children : (c \in psent) ~> (st[c] = "dead")
\* Down exactly once: a fired or late monitor delivers its Down unless the
\* observer died first.
DownDelivered ==
    \A m \in MonIds : (mstate[m] \in {"fired", "late"}) ~>
        (downs[m] = 1 \/ (mon[m].o \in Actors /\ st[mon[m].o] = "dead"))
\* An abnormal exit notice reaches its linked peer: a non-trapping peer
\* stops, a trapping one gets an Exit (or a parent exit) message.
LinkedPeersStop ==
    \A a, b \in Actors :
        (\E r \in Reasons \ {"Normal"} : Note(b, a, "link", r, 0, FALSE) \in inflight) ~>
            (st[b] # "run" \/ \E i \in 1..Len(mbox[b]) : mbox[b][i].s = a)
=============================================================================
