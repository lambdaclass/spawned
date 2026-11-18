------------------------------ MODULE Disposal ------------------------------
(***************************************************************************)
(* The deadline service's hand-off of user code to disposal workers        *)
(* (concurrency/src/reply/disposal.rs, deadline.rs, registry.rs, and       *)
(* DropFailures in mailbox.rs; docs/design/deferred-replies.md §2,         *)
(* "Callback progress"). A companion of DeferredReplies.tla, which models  *)
(* the reply slots themselves and takes the service's expiry as one step.  *)
(*                                                                         *)
(* One owner's deadline heap holds N requests (deferred tokens or awaited  *)
(* raw receivers), each with a deadline fixed at Init and possibly a       *)
(* waiting caller (a waker). The one process-wide service thread visits    *)
(* the heap and collects what it releases in the visit's batch:            *)
(*   Expire(r)    the slot's OPEN -> TIMED_OUT transition and its credit   *)
(*                return, in one step; a waiting caller's wake-up item     *)
(*   Release(p)   a payload (a discarded scheduled emit, a displaced job  *)
(*                result, `done` and its captures); one released while the *)
(*                owner's hold is on is held at once and is no step        *)
(*   Other(o)     anything else whose drop or call may run user code (the  *)
(*                last reference to a job or registry, a job's emitter     *)
(*                wakers, a slot holding an unread reply)                  *)
(*   Submit       the batch goes to the pool, one queue entry per item;   *)
(*                then the start rule                                      *)
(* The pool mirrors run()/submit(): a worker is "none" (not started),     *)
(* "active" (in its loop, holding the lock), "idle" (waiting: counted in  *)
(* `idle`), "taken" (holds an entry), "in" (runs its next item) or         *)
(* "settled" (a payload's hold check is done, its destructor runs). The    *)
(* start rule (`needs_worker`): when queued entries outnumber idle         *)
(* workers, none is starting and fewer than Workers exist (the cap), one  *)
(* starts; it is checked after Submit and after every Take. A payload's   *)
(* run is two steps, as `DropFailures::settle`: Settle decides under the   *)
(* hold lock (held if the owner's exit began, else dropped) and the        *)
(* destructor then runs until Done. Items in Blocking never reach Done (a *)
(* blocking waker or destructor): their worker is lost.                    *)
(*                                                                         *)
(* The owner: Exit (DropFailures::hold, its exit begins) and Death (the    *)
(* death point; DropFailures::release drops what was held). A destructor   *)
(* whose drop was decided before Exit may still run during the exit: it   *)
(* is pre-exit work and Exit never waits for it (deferred-replies.md §5);  *)
(* NoDropWhileHeld is about the decision.                                  *)
(*                                                                         *)
(* The clock: Tick only when no framework step is pending (an expiry due,  *)
(* a batch not submitted, a worker that can start, take or run), so a      *)
(* property "within the tick" says the step does not wait for a blocked   *)
(* item.                                                                   *)
(*                                                                         *)
(* Fault selects a negative control; negative_checks.py requires each to   *)
(* violate the named invariant:                                            *)
(*   InlineDisposal       (TimelyExpiry)   the service runs the batch      *)
(*                                         itself (0.8.0)                  *)
(*   WakeBeforeTransition (WakeAfterTransition) the wake-up is handed off  *)
(*                                         before the transition commits   *)
(*   DropWhileHeld        (NoDropWhileHeld) the worker drops a payload     *)
(*                                         without re-checking the hold    *)
(*   BatchSerial          (TimelyWake)     a visit's batch is one entry    *)
(*                                         run serially (the shared-batch  *)
(*                                         bug of PR #282's first rev.)    *)
(*   NoStartOnTake        (TimelyWake)     the start rule is checked only  *)
(*                                         at Submit: a queued item can be *)
(*                                         stranded behind blocked workers *)
(***************************************************************************)
EXTENDS Naturals, Sequences, FiniteSets

CONSTANTS N, P, O, MaxTime, Workers, Blocking, Fault, InitIdle
ASSUME Fault \in {"None", "InlineDisposal", "WakeBeforeTransition",
                  "DropWhileHeld", "BatchSerial", "NoStartOnTake"}

Req == 1..N
Pay == 1..P
Oth == 1..O
W == 1..Workers
ASSUME InitIdle \subseteq W
WakeItem(r) == [k |-> "wake", id |-> r]
DropItem(p) == [k |-> "drop", id |-> p]
OtherItem(o) == [k |-> "other", id |-> o]
Items == {WakeItem(r) : r \in Req} \cup {DropItem(p) : p \in Pay}
         \cup {OtherItem(o) : o \in Oth}
ASSUME Blocking \subseteq Items

VARIABLES
    now, dl, waiting,      \* the clock; each request's deadline and caller
    slot, credit, expAt,   \* "open"/"timeout"; its credit; when it expired
    woken,                 \* the caller's wake-up ran
    cas,                   \* WakeBeforeTransition: handed off, not committed
    payload,               \* "none", "out", "held", "dropping", "dropped"
    other,                 \* "none", "out", "done"
    batch,                 \* the current visit's collected items
    queue,                 \* queued entries (each a sequence of items)
    ws,                    \* each worker's state (see above)
    cur,                   \* the entry a worker holds (<<>> if none)
    starting,              \* a worker start was decided, not yet begun
    svc,                   \* "idle", or "stuck" in a blocking item (InlineDisposal)
    phase,                 \* the owner: "run", "exiting" (hold on), "dead"
    early                  \* a payload's drop was decided while the hold was on

vars == <<now, dl, waiting, slot, credit, expAt, woken, cas, payload, other,
          batch, queue, ws, cur, starting, svc, phase, early>>
pool == <<queue, ws, cur, starting>>

Init ==
    /\ now = 0
    /\ dl \in [Req -> 1..MaxTime]
    /\ waiting \in [Req -> BOOLEAN]
    /\ slot = [r \in Req |-> "open"]
    /\ credit = [r \in Req |-> TRUE]
    /\ expAt = [r \in Req |-> 0]
    /\ woken = [r \in Req |-> FALSE]
    /\ cas = {}
    /\ payload = [p \in Pay |-> "none"]
    /\ other = [o \in Oth |-> "none"]
    /\ batch = <<>>
    /\ queue = <<>>
    /\ ws = [w \in W |-> IF w \in InitIdle THEN "idle" ELSE "none"]
    /\ cur = [w \in W |-> <<>>]
    /\ starting = FALSE
    /\ svc = "idle"
    /\ phase = "run"
    /\ early = FALSE

------------------------------------------------------------------------------
\* The pool's bookkeeping (disposal.rs `State`)

IdleCount == Cardinality({w \in W : ws[w] = "idle"})
Started == {w \in W : ws[w] # "none"}
\* `needs_worker` on queue `q` with `idle` idle workers.
NeedsWorker(q, idle) ==
    /\ Len(q) > idle /\ ~starting
    /\ Cardinality(Started) < Workers

------------------------------------------------------------------------------
\* The service thread

RunInline(item) == IF item \in Blocking THEN svc' = "stuck" ELSE UNCHANGED svc

Due(r) == slot[r] = "open" /\ r \notin cas /\ now >= dl[r]

Expire(r) ==
    /\ svc = "idle" /\ Due(r)
    /\ IF Fault = "WakeBeforeTransition"
       THEN /\ cas' = cas \cup {r}
            /\ batch' = IF waiting[r] THEN Append(batch, WakeItem(r)) ELSE batch
            /\ UNCHANGED <<slot, credit, expAt, woken, svc>>
       ELSE /\ slot' = [slot EXCEPT ![r] = "timeout"]
            /\ credit' = [credit EXCEPT ![r] = FALSE]
            /\ expAt' = [expAt EXCEPT ![r] = now]
            /\ UNCHANGED cas
            /\ IF Fault = "InlineDisposal"
               THEN /\ UNCHANGED batch
                    /\ IF waiting[r] /\ WakeItem(r) \notin Blocking
                       THEN woken' = [woken EXCEPT ![r] = TRUE] ELSE UNCHANGED woken
                    /\ IF waiting[r] THEN RunInline(WakeItem(r)) ELSE UNCHANGED svc
               ELSE /\ batch' = IF waiting[r] THEN Append(batch, WakeItem(r)) ELSE batch
                    /\ UNCHANGED <<woken, svc>>
    /\ UNCHANGED <<now, dl, waiting, payload, other, pool, phase, early>>

\* WakeBeforeTransition only: the transition commits after the hand-off.
Commit(r) ==
    /\ r \in cas
    /\ cas' = cas \ {r}
    /\ slot' = [slot EXCEPT ![r] = "timeout"]
    /\ credit' = [credit EXCEPT ![r] = FALSE]
    /\ expAt' = [expAt EXCEPT ![r] = now]
    /\ UNCHANGED <<now, dl, waiting, woken, payload, other, batch, pool, svc, phase, early>>

Release(p) ==
    /\ svc = "idle" /\ payload[p] = "none" /\ phase # "exiting"
    /\ IF Fault = "InlineDisposal"
       THEN /\ payload' = [payload EXCEPT ![p] = IF DropItem(p) \in Blocking THEN "dropping" ELSE "dropped"]
            /\ RunInline(DropItem(p))
            /\ UNCHANGED batch
       ELSE /\ payload' = [payload EXCEPT ![p] = "out"]
            /\ batch' = Append(batch, DropItem(p))
            /\ UNCHANGED svc
    /\ UNCHANGED <<now, dl, waiting, slot, credit, expAt, woken, cas, other, pool, phase, early>>

Other(o) ==
    /\ svc = "idle" /\ other[o] = "none"
    /\ IF Fault = "InlineDisposal"
       THEN /\ other' = [other EXCEPT ![o] = IF OtherItem(o) \in Blocking THEN "out" ELSE "done"]
            /\ RunInline(OtherItem(o))
            /\ UNCHANGED batch
       ELSE /\ other' = [other EXCEPT ![o] = "out"]
            /\ batch' = Append(batch, OtherItem(o))
            /\ UNCHANGED svc
    /\ UNCHANGED <<now, dl, waiting, slot, credit, expAt, woken, cas, payload, pool, phase, early>>

\* One queue entry per item; BatchSerial: the whole batch as one entry.
Entries(b) == IF Fault = "BatchSerial" THEN <<b>> ELSE [i \in 1..Len(b) |-> <<b[i]>>]

\* `spawn`: whether the code decided to start a worker (the trace's x).
SubmitDecide(spawn) ==
    /\ batch # <<>>
    /\ queue' = queue \o Entries(batch)
    /\ batch' = <<>>
    /\ starting' = (starting \/ NeedsWorker(queue', IdleCount))
    /\ spawn = (starting' /\ ~starting)
    /\ UNCHANGED <<now, dl, waiting, slot, credit, expAt, woken, cas, payload, other, ws, cur, svc, phase, early>>
Submit == \E spawn \in BOOLEAN : SubmitDecide(spawn)

------------------------------------------------------------------------------
\* Disposal workers (disposal.rs `run`)

\* A started thread enters its loop (`starting = false`).
Begin(w) ==
    /\ ws[w] = "none" /\ starting
    /\ ws' = [ws EXCEPT ![w] = "active"] /\ starting' = FALSE
    /\ UNCHANGED <<now, dl, waiting, slot, credit, expAt, woken, cas, payload, other, batch, queue, cur, svc, phase, early>>

\* Pop an entry; then the start rule for what is still queued.
TakeDecide(w, spawn) ==
    /\ ws[w] = "active" /\ queue # <<>>
    /\ ws' = [ws EXCEPT ![w] = "taken"]
    /\ cur' = [cur EXCEPT ![w] = Head(queue)]
    /\ queue' = Tail(queue)
    /\ starting' = (starting \/ (Fault # "NoStartOnTake" /\ NeedsWorker(Tail(queue), IdleCount)))
    /\ spawn = (starting' /\ ~starting)
    /\ UNCHANGED <<now, dl, waiting, slot, credit, expAt, woken, cas, payload, other, batch, svc, phase, early>>
Take(w) == \E spawn \in BOOLEAN : TakeDecide(w, spawn)

\* Nothing queued: wait (counted idle).
Wait(w) ==
    /\ ws[w] = "active" /\ queue = <<>>
    /\ ws' = [ws EXCEPT ![w] = "idle"]
    /\ UNCHANGED <<now, dl, waiting, slot, credit, expAt, woken, cas, payload, other, batch, queue, cur, starting, svc, phase, early>>

\* Back under the lock after a notification, a time-out or spuriously.
Woke(w) ==
    /\ ws[w] = "idle"
    /\ ws' = [ws EXCEPT ![w] = "active"]
    /\ UNCHANGED <<now, dl, waiting, slot, credit, expAt, woken, cas, payload, other, batch, queue, cur, starting, svc, phase, early>>

Retire(w) ==
    /\ ws[w] = "active" /\ queue = <<>>
    /\ ws' = [ws EXCEPT ![w] = "none"]
    /\ UNCHANGED <<now, dl, waiting, slot, credit, expAt, woken, cas, payload, other, batch, queue, cur, starting, svc, phase, early>>

\* Start the entry's next item: a wake-up wakes its caller here.
Run(w) ==
    /\ ws[w] = "taken"
    /\ ws' = [ws EXCEPT ![w] = "in"]
    /\ LET item == Head(cur[w]) IN
          IF item.k = "wake" THEN woken' = [woken EXCEPT ![item.id] = TRUE]
          ELSE UNCHANGED woken
    /\ UNCHANGED <<now, dl, waiting, slot, credit, expAt, cas, payload, other, batch, queue, cur, starting, svc, phase, early>>

\* A payload's hold check (`DropFailures::settle`, under the hold lock).
SettleTo(w, x) ==
    /\ ws[w] = "in" /\ Head(cur[w]).k = "drop"
    /\ LET p == Head(cur[w]).id
           held == phase = "exiting" /\ Fault # "DropWhileHeld" IN
       /\ x = IF held THEN "held" ELSE "dropped"
       /\ payload' = [payload EXCEPT ![p] = IF held THEN "held" ELSE "dropping"]
       /\ early' = (early \/ (~held /\ phase = "exiting"))
    /\ ws' = [ws EXCEPT ![w] = "settled"]
    /\ UNCHANGED <<now, dl, waiting, slot, credit, expAt, woken, cas, other, batch, queue, cur, starting, svc, phase>>
Settle(w) == \E x \in {"held", "dropped"} : SettleTo(w, x)

\* The item returns (a blocking one never does).
Done(w) ==
    /\ \/ ws[w] = "in" /\ Head(cur[w]).k # "drop"
       \/ ws[w] = "settled"
    /\ LET item == Head(cur[w]) IN
       /\ item \notin Blocking
       /\ IF item.k = "drop" /\ payload[item.id] = "dropping"
          THEN payload' = [payload EXCEPT ![item.id] = "dropped"] ELSE UNCHANGED payload
       /\ IF item.k = "other" THEN other' = [other EXCEPT ![item.id] = "done"] ELSE UNCHANGED other
    /\ cur' = [cur EXCEPT ![w] = Tail(cur[w])]
    /\ ws' = [ws EXCEPT ![w] = IF Tail(cur[w]) = <<>> THEN "active" ELSE "taken"]
    /\ UNCHANGED <<now, dl, waiting, slot, credit, expAt, woken, cas, batch, queue, starting, svc, phase, early>>

------------------------------------------------------------------------------
\* The owner

Exit ==
    /\ phase = "run" /\ phase' = "exiting"
    /\ UNCHANGED <<now, dl, waiting, slot, credit, expAt, woken, cas, payload, other, batch, pool, svc, early>>

\* The death point: what was held is dropped.
Death ==
    /\ phase = "exiting" /\ phase' = "dead"
    /\ payload' = [p \in Pay |-> IF payload[p] = "held" THEN "dropped" ELSE payload[p]]
    /\ UNCHANGED <<now, dl, waiting, slot, credit, expAt, woken, cas, other, batch, pool, svc, early>>

------------------------------------------------------------------------------
\* The clock

Stuck(w) == ws[w] \in {"in", "settled"} /\ Head(cur[w]) \in Blocking

\* A framework step is pending (it never waits for user code).
Pending ==
    \/ svc = "idle" /\ \E r \in Req : Due(r)
    \/ cas # {}
    \/ batch # <<>>
    \/ starting
    \/ \E w \in W : ws[w] \in {"active", "taken"}
    \/ \E w \in W : ws[w] \in {"in", "settled"} /\ ~Stuck(w)
    \/ queue # <<>> /\ \E w \in W : ws[w] = "idle"

Tick ==
    /\ now < MaxTime /\ ~Pending
    /\ now' = now + 1
    /\ UNCHANGED <<dl, waiting, slot, credit, expAt, woken, cas, payload, other, batch, pool, svc, phase, early>>

Next ==
    \/ Tick \/ Submit \/ Exit \/ Death
    \/ \E r \in Req : Expire(r) \/ Commit(r)
    \/ \E p \in Pay : Release(p)
    \/ \E o \in Oth : Other(o)
    \/ \E w \in W : Begin(w) \/ Take(w) \/ Wait(w) \/ Woke(w) \/ Retire(w)
                    \/ Run(w) \/ Settle(w) \/ Done(w)

Spec == Init /\ [][Next]_vars

\* Fairness for the clock, the service and the workers (framework code; an
\* idle worker is notified when work is queued); none for the owner, for
\* Retire, or for a blocking item (Done is not enabled for it).
FairSpec ==
    /\ Spec /\ WF_vars(Tick) /\ WF_vars(Submit)
    /\ \A r \in Req : WF_vars(Expire(r)) /\ WF_vars(Commit(r))
    /\ \A w \in W : /\ WF_vars(Begin(w)) /\ WF_vars(Take(w)) /\ WF_vars(Wait(w))
                    /\ WF_vars(Woke(w) /\ queue # <<>>)
                    /\ WF_vars(Run(w)) /\ WF_vars(Settle(w)) /\ WF_vars(Done(w))

------------------------------------------------------------------------------
\* Properties

TypeOK ==
    /\ now \in 0..MaxTime
    /\ slot \in [Req -> {"open", "timeout"}]
    /\ credit \in [Req -> BOOLEAN] /\ woken \in [Req -> BOOLEAN]
    /\ payload \in [Pay -> {"none", "out", "held", "dropping", "dropped"}]
    /\ other \in [Oth -> {"none", "out", "done"}]
    /\ ws \in [W -> {"none", "active", "idle", "taken", "in", "settled"}]
    /\ cas \subseteq Req /\ starting \in BOOLEAN
    /\ phase \in {"run", "exiting", "dead"} /\ svc \in {"idle", "stuck"}

\* A deferral credit is held exactly while the slot is open: it returns with
\* the transition, on the service thread, never with a disposal.
CreditWithTransition == \A r \in Req : credit[r] = (slot[r] = "open")
\* No wake-up runs before its slot's transition and credit return.
WakeAfterTransition == \A r \in Req : woken[r] => (slot[r] = "timeout" /\ ~credit[r])
\* No handed-off payload's drop is decided while the owner's hold is on: it
\* is held and dropped at the death point (R6, callers before payloads).
NoDropWhileHeld == ~early
\* The cap: never more than Workers started or starting.
WithinCap == Cardinality(Started) + (IF starting THEN 1 ELSE 0) <= Workers
\* The service does not wait for user code.
TimelyExpiry == \A r \in Req : slot[r] = "open" => now <= dl[r]
\* A wake-up that does not itself block runs within the tick of its expiry,
\* whatever else blocks (another item of the same visit included).
TimelyWake ==
    \A r \in Req : (slot[r] = "timeout" /\ waiting[r] /\ WakeItem(r) \notin Blocking
                    /\ now > expAt[r]) => woken[r]
\* At the cap: a late wake-up only while every worker is stuck in a
\* blocking item (MCDisposalCapped).
TimelyWakeBelowCap ==
    \A r \in Req : (slot[r] = "timeout" /\ waiting[r] /\ WakeItem(r) \notin Blocking
                    /\ now > expAt[r] /\ ~woken[r])
                   => \A w \in W : Stuck(w)

Safety ==
    /\ TypeOK /\ CreditWithTransition /\ WakeAfterTransition /\ NoDropWhileHeld
    /\ WithinCap /\ TimelyExpiry /\ TimelyWake

SafetyCapped ==
    /\ TypeOK /\ CreditWithTransition /\ WakeAfterTransition /\ NoDropWhileHeld
    /\ WithinCap /\ TimelyExpiry /\ TimelyWakeBelowCap

\* Every handed-off wake-up that does not block eventually runs.
EveryWakeRuns ==
    \A r \in Req : (slot[r] = "timeout" /\ waiting[r] /\ WakeItem(r) \notin Blocking)
                   ~> woken[r]
\* Every deadline is eventually serviced.
EveryDueExpires == \A r \in Req : (now >= dl[r]) ~> (slot[r] = "timeout")
=============================================================================
