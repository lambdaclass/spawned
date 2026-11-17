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
(* the heap:                                                               *)
(*   Expire(r)   the slot's OPEN -> TIMED_OUT transition and its credit    *)
(*               return, in one step; a waiting caller's wake-up is put    *)
(*               in the visit's batch (not run)                            *)
(*   Release(p)  a payload the visit releases (a discarded scheduled emit, *)
(*               a displaced job result, `done` and its captures) is put   *)
(*               in the batch; one released while the owner's hold is on   *)
(*               is held at once and is no step (DropFailures::discard)    *)
(*   Submit      the visit's batch goes to the pool, one queue entry per   *)
(*               item                                                      *)
(* Workers (at most Workers) are started while queued entries outnumber    *)
(* idle workers, take entries, run them, and retire when idle with nothing *)
(* queued. Running a wake-up wakes the caller; running a payload re-checks *)
(* the owner's hold: held if its exit began (dropped at its death point),  *)
(* dropped otherwise. Items in Blocking never return (a blocking waker or  *)
(* destructor): their worker is lost, nothing else waits for it.           *)
(*                                                                         *)
(* The owner: Exit (DropFailures::hold, its exit begins) and Death (the    *)
(* death point; DropFailures::release drops what was held).                *)
(*                                                                         *)
(* The clock: Tick only when no framework step is pending (an expiry due,  *)
(* a batch not submitted, a runnable or startable worker), so a property   *)
(* "within the tick" says the step does not wait for a blocked item.       *)
(*                                                                         *)
(* Fault selects a negative control; negative_checks.py requires each to   *)
(* violate the named invariant:                                            *)
(*   InlineDisposal       (TimelyExpiry)   the service runs the batch      *)
(*                                         itself (0.8.0): a blocking item *)
(*                                         stops every later expiry        *)
(*   WakeBeforeTransition (WakeAfterTransition) the wake-up is handed off  *)
(*                                         before the transition commits   *)
(*   DropWhileHeld        (NoDropWhileHeld) the worker drops a payload     *)
(*                                         without re-checking the hold    *)
(*   BatchSerial          (TimelyWake)     a visit's batch is one queue    *)
(*                                         entry run serially: a blocking  *)
(*                                         item holds back the other       *)
(*                                         callers' wake-ups (review of    *)
(*                                         PR #282)                        *)
(***************************************************************************)
EXTENDS Naturals, Sequences, FiniteSets

CONSTANTS N, P, MaxTime, Workers, Blocking, Fault
ASSUME Fault \in {"None", "InlineDisposal", "WakeBeforeTransition",
                  "DropWhileHeld", "BatchSerial"}

Req == 1..N
Pay == 1..P
W == 1..Workers
WakeItem(r) == [k |-> "wake", id |-> r]
DropItem(p) == [k |-> "drop", id |-> p]
Items == {WakeItem(r) : r \in Req} \cup {DropItem(p) : p \in Pay}
ASSUME Blocking \subseteq Items

VARIABLES
    now, dl, waiting,      \* the clock; each request's deadline and caller
    slot, credit, expAt,   \* "open"/"timeout"; its credit; when it expired
    woken,                 \* the caller's wake-up ran
    cas,                   \* WakeBeforeTransition: handed off, not committed
    payload,               \* "none" (owned), "out" (handed off), "held", "dropped"
    batch,                 \* the current visit's collected items
    queue,                 \* queued entries (each a sequence of items)
    cur,                   \* the entry a worker runs (<<>>: idle or not started)
    alive,                 \* started workers
    svc,                   \* "idle", or "stuck" in a blocking item (InlineDisposal)
    phase,                 \* the owner: "run", "exiting" (hold on), "dead"
    early                  \* a payload was dropped while the hold was on

vars == <<now, dl, waiting, slot, credit, expAt, woken, cas, payload, batch,
          queue, cur, alive, svc, phase, early>>

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
    /\ batch = <<>>
    /\ queue = <<>>
    /\ cur = [w \in W |-> <<>>]
    /\ alive = {}
    /\ svc = "idle"
    /\ phase = "run"
    /\ early = FALSE

------------------------------------------------------------------------------
\* The service thread

\* InlineDisposal: the service runs the item itself.
RunInline(item) ==
    IF item \in Blocking THEN svc' = "stuck" ELSE UNCHANGED svc

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
    /\ UNCHANGED <<now, dl, waiting, payload, queue, cur, alive, phase, early>>

\* WakeBeforeTransition only: the transition commits after the hand-off.
Commit(r) ==
    /\ r \in cas
    /\ cas' = cas \ {r}
    /\ slot' = [slot EXCEPT ![r] = "timeout"]
    /\ credit' = [credit EXCEPT ![r] = FALSE]
    /\ expAt' = [expAt EXCEPT ![r] = now]
    /\ UNCHANGED <<now, dl, waiting, woken, payload, batch, queue, cur, alive, svc, phase, early>>

Release(p) ==
    /\ svc = "idle" /\ payload[p] = "none" /\ phase # "exiting"
    /\ IF Fault = "InlineDisposal"
       THEN /\ payload' = [payload EXCEPT ![p] = IF DropItem(p) \in Blocking THEN "out" ELSE "dropped"]
            /\ RunInline(DropItem(p))
            /\ UNCHANGED batch
       ELSE /\ payload' = [payload EXCEPT ![p] = "out"]
            /\ batch' = Append(batch, DropItem(p))
            /\ UNCHANGED svc
    /\ UNCHANGED <<now, dl, waiting, slot, credit, expAt, woken, cas, queue, cur, alive, phase, early>>

\* One queue entry per item; BatchSerial: the whole batch as one entry.
Entries(b) == IF Fault = "BatchSerial" THEN <<b>> ELSE [i \in 1..Len(b) |-> <<b[i]>>]

Submit ==
    /\ batch # <<>>
    /\ queue' = queue \o Entries(batch)
    /\ batch' = <<>>
    /\ UNCHANGED <<now, dl, waiting, slot, credit, expAt, woken, cas, payload, cur, alive, svc, phase, early>>

------------------------------------------------------------------------------
\* Disposal workers

Idle(w) == w \in alive /\ cur[w] = <<>>
IdleCount == Cardinality({w \in W : Idle(w)})

Start(w) ==
    /\ w \notin alive /\ Len(queue) > IdleCount
    /\ alive' = alive \cup {w}
    /\ UNCHANGED <<now, dl, waiting, slot, credit, expAt, woken, cas, payload, batch, queue, cur, svc, phase, early>>

Take(w) ==
    /\ Idle(w) /\ queue # <<>>
    /\ cur' = [cur EXCEPT ![w] = Head(queue)]
    /\ queue' = Tail(queue)
    /\ UNCHANGED <<now, dl, waiting, slot, credit, expAt, woken, cas, payload, batch, alive, svc, phase, early>>

\* Run the worker's next item; a blocking one never returns.
Run(w) ==
    /\ cur[w] # <<>>
    /\ LET item == Head(cur[w]) IN
       /\ item \notin Blocking
       /\ cur' = [cur EXCEPT ![w] = Tail(cur[w])]
       /\ IF item.k = "wake"
          THEN /\ woken' = [woken EXCEPT ![item.id] = TRUE]
               /\ UNCHANGED <<payload, early>>
          ELSE /\ IF phase = "exiting" /\ Fault # "DropWhileHeld"
                  THEN /\ payload' = [payload EXCEPT ![item.id] = "held"]
                       /\ UNCHANGED early
                  ELSE /\ payload' = [payload EXCEPT ![item.id] = "dropped"]
                       /\ early' = (early \/ phase = "exiting")
               /\ UNCHANGED woken
    /\ UNCHANGED <<now, dl, waiting, slot, credit, expAt, cas, batch, queue, alive, svc, phase>>

Retire(w) ==
    /\ Idle(w) /\ queue = <<>>
    /\ alive' = alive \ {w}
    /\ UNCHANGED <<now, dl, waiting, slot, credit, expAt, woken, cas, payload, batch, queue, cur, svc, phase, early>>

------------------------------------------------------------------------------
\* The owner

Exit ==
    /\ phase = "run" /\ phase' = "exiting"
    /\ UNCHANGED <<now, dl, waiting, slot, credit, expAt, woken, cas, payload, batch, queue, cur, alive, svc, early>>

\* The death point: what was held is dropped.
Death ==
    /\ phase = "exiting" /\ phase' = "dead"
    /\ payload' = [p \in Pay |-> IF payload[p] = "held" THEN "dropped" ELSE payload[p]]
    /\ UNCHANGED <<now, dl, waiting, slot, credit, expAt, woken, cas, batch, queue, cur, alive, svc, early>>

------------------------------------------------------------------------------
\* The clock

\* A framework step is pending (it never waits for user code).
Pending ==
    \/ svc = "idle" /\ \E r \in Req : Due(r)
    \/ cas # {}
    \/ batch # <<>>
    \/ \E w \in W : cur[w] # <<>> /\ Head(cur[w]) \notin Blocking
    \/ queue # <<>> /\ \E w \in W : Idle(w) \/ w \notin alive

Tick ==
    /\ now < MaxTime /\ ~Pending
    /\ now' = now + 1
    /\ UNCHANGED <<dl, waiting, slot, credit, expAt, woken, cas, payload, batch, queue, cur, alive, svc, phase, early>>

Next ==
    \/ Tick \/ Submit \/ Exit \/ Death
    \/ \E r \in Req : Expire(r) \/ Commit(r)
    \/ \E p \in Pay : Release(p)
    \/ \E w \in W : Start(w) \/ Take(w) \/ Run(w) \/ Retire(w)

Spec == Init /\ [][Next]_vars

\* Fairness for the clock, the service and the workers (framework code);
\* none for the owner or for a blocking item (Run is not enabled for it).
FairSpec == Spec /\ WF_vars(Tick) /\ WF_vars(Submit)
            /\ \A r \in Req : WF_vars(Expire(r)) /\ WF_vars(Commit(r))
            /\ \A w \in W : WF_vars(Start(w)) /\ WF_vars(Take(w)) /\ WF_vars(Run(w))

------------------------------------------------------------------------------
\* Properties

TypeOK ==
    /\ now \in 0..MaxTime
    /\ slot \in [Req -> {"open", "timeout"}]
    /\ credit \in [Req -> BOOLEAN] /\ woken \in [Req -> BOOLEAN]
    /\ payload \in [Pay -> {"none", "out", "held", "dropped"}]
    /\ cas \subseteq Req /\ alive \subseteq W
    /\ phase \in {"run", "exiting", "dead"} /\ svc \in {"idle", "stuck"}

\* A deferral credit is held exactly while the slot is open: it returns with
\* the transition, on the service thread, never with a disposal.
CreditWithTransition == \A r \in Req : credit[r] = (slot[r] = "open")
\* No wake-up runs before its slot's transition and credit return.
WakeAfterTransition == \A r \in Req : woken[r] => (slot[r] = "timeout" /\ ~credit[r])
\* No payload handed off is dropped while the owner's hold is on: it waits
\* for the death point (R6, callers before payloads).
NoDropWhileHeld == ~early
\* The service does not wait for user code: no due slot stays open past its
\* deadline (the clock passes it only if the service is stuck).
TimelyExpiry == \A r \in Req : slot[r] = "open" => now <= dl[r]
\* A wake-up that does not itself block runs within the tick of its expiry,
\* whatever else blocks (another item of the same visit included).
TimelyWake ==
    \A r \in Req : (slot[r] = "timeout" /\ waiting[r] /\ WakeItem(r) \notin Blocking
                    /\ now > expAt[r]) => woken[r]

Safety ==
    /\ TypeOK /\ CreditWithTransition /\ WakeAfterTransition /\ NoDropWhileHeld
    /\ TimelyExpiry /\ TimelyWake

\* Every handed-off wake-up that does not block eventually runs.
EveryWakeRuns ==
    \A r \in Req : (slot[r] = "timeout" /\ waiting[r] /\ WakeItem(r) \notin Blocking)
                   ~> woken[r]
\* Every deadline is eventually serviced.
EveryDueExpires == \A r \in Req : (now >= dl[r]) ~> (slot[r] = "timeout")
=============================================================================
