----------------------------- MODULE Admission -----------------------------
(***************************************************************************)
(* Admission package design model (docs/design/admission.md, not          *)
(* implemented): one actor's bounded mailbox (rank 17) with weighted      *)
(* credits (rank 18), opt-in skipping of expired requests (rank 19) and   *)
(* the opt-in priority lane (rank 14, G11).                               *)
(*                                                                         *)
(* The mailbox draws credits from a pool. A message of weight w needs w   *)
(* free credits (weight 1 everywhere is a slot count, a byte estimate is  *)
(* rank 18). The pool may be shared with another mailbox (a pipeline      *)
(* budget); that mailbox is the environment `ow`. A sender either never   *)
(* waits (`send`, `try_send`, `send_request`) or waits under its deadline *)
(* (`send_wait`, `request`). Waiters queue FIFO on the pool; a release    *)
(* hands credits to the head waiter in the same step, so free credits and *)
(* a head waiter that fits never coexist and a later sender cannot barge. *)
(*                                                                         *)
(* Framework notices (Down, trapped Exit) take no credit and are never    *)
(* refused; each edge delivers at most one. An edge the receiver opted in *)
(* as priority delivers to the priority lane, dispatched before the       *)
(* ordinary lane between turns, at most Burst times in a row while an     *)
(* ordinary item waits (the valve).                                       *)
(*                                                                         *)
(* Stop (D4) ends dispatch; the death point D answers queued requests and *)
(* request waiters (ActorStopped), releases send waiters (NoProc), and    *)
(* returns every credit the instance holds to the pool.                   *)
(*                                                                         *)
(* Outcomes: "accepted" (a send admitted), "admitted" (a request admitted,*)
(* reply slot open), "full" (never admitted: MailboxFull at once or at    *)
(* the wait's deadline), "toolarge", "noproc", "ok", "timeout" (the       *)
(* caller's deadline passed after admission: RequestTimeout), "stopped"   *)
(* (ActorStopped), "dropped" (ReplyDropped).                              *)
(*                                                                         *)
(* Fault = "None" or a negative control that must fail (negative_checks.py*)
(* ADMISSION_CONTROLS, each on its named invariant or property):          *)
(*   Barge, FirstFit, LazyHandoff, OversizeWaits, NoCapacityCheck,        *)
(*   ExpireRacesHandoff, TimeoutWhileWaiting, LateAdmit, KeepWaiters,     *)
(*   LeakCredits, CountControl, SkipOpen, SkipNoOptIn, NoValve            *)
(***************************************************************************)
EXTENDS Naturals, Sequences, FiniteSets

CONSTANTS
    Senders,    \* external senders
    Weight,     \* [Senders -> Nat]: credits one message of that sender needs
    Kind,       \* [Senders -> {"send", "req"}]
    Mode,       \* [Senders -> {"try", "wait"}]
    Skip,       \* [Senders -> BOOLEAN]: the request's handler opted in to skip_expired
    MaxMsgs,    \* messages per sender
    Limit,      \* the pool's credits
    OtherMax,   \* credits another mailbox on the same pool may hold (0: private pool)
    Edges,      \* link and monitor edges into this actor
    Prio,       \* edges the receiver opted in as priority (G11)
    Burst,      \* the valve
    Fault

ASSUME Prio \subseteq Edges /\ Burst >= 1

Ids == Senders \X (1..MaxMsgs)
CtlIds == Edges \X {0}
NoItem == [id |-> <<"none", 0>>, src |-> "none", w |-> 0, kind |-> "none"]
Item(s, n) == [id |-> <<s, n>>, src |-> s, w |-> Weight[s], kind |-> Kind[s]]
Ctl(e) == [id |-> <<e, 0>>, src |-> e, w |-> 0, kind |-> "ctl"]
Outcomes == {"none", "accepted", "admitted", "full", "toolarge", "noproc",
             "ok", "timeout", "stopped", "dropped"}

VARIABLES
    ph,         \* "run", "stopping" (stop requested, D4), "dead" (past D)
    cur,        \* the item whose turn runs, or NoItem
    q,          \* the ordinary lane: admitted messages and ordinary notices
    cq,         \* the priority lane
    waiters,    \* senders waiting for credits, FIFO
    used,       \* credits this mailbox holds
    ow,         \* credits the other mailbox on the pool holds
    sent,       \* messages each sender has begun
    out,        \* each message's outcome
    handled,    \* ids whose handler ran
    skipped,    \* ids skipped at dispatch (skip_expired)
    arrived,    \* edges whose notice arrived
    overtake,   \* priority turns in a row while the ordinary lane was not empty
    \* ghosts, derived in Next from state changes
    admitted,   \* ids ever admitted
    lost,       \* edges whose notice was refused
    overtaken   \* waiting ids passed over by a later admission

vars == <<ph, cur, q, cq, waiters, used, ow, sent, out, handled, skipped,
          arrived, overtake, admitted, lost, overtaken>>

Init ==
    /\ ph = "run" /\ cur = NoItem /\ q = <<>> /\ cq = <<>> /\ waiters = <<>>
    /\ used = 0 /\ ow = 0 /\ sent = [s \in Senders |-> 0]
    /\ out = [i \in Ids |-> "none"] /\ handled = {} /\ skipped = {}
    /\ arrived = {} /\ overtake = 0 /\ admitted = {} /\ lost = {}
    /\ overtaken = {}

-----------------------------------------------------------------------------
\* Helpers

SeqIds(s) == {s[k].id : k \in DOMAIN s}
InWaiters(i) == i \in SeqIds(waiters)
Waiting(s) == sent[s] > 0 /\ InWaiters(<<s, sent[s]>>)
Busy(s) == sent[s] > 0 /\ out[<<s, sent[s]>>] = "admitted"
Free(s) == sent[s] < MaxMsgs /\ ~Waiting(s) /\ ~Busy(s)
Fits(u, o, w) == u + o + w <= Limit
Remove(s, k) == SubSeq(s, 1, k - 1) \o SubSeq(s, k + 1, Len(s))
RECURSIVE SumW(_)
SumW(s) == IF s = <<>> THEN 0 ELSE Head(s).w + SumW(Tail(s))
Accepted(s) == IF Kind[s] = "req" THEN "admitted" ELSE "accepted"

\* The hand-off: admit head waiters while the head fits.
RECURSIVE Fill(_, _, _, _)
Fill(qq, u, o, ws) ==
    IF ws # <<>> /\ Fits(u, o, Head(ws).w)
    THEN Fill(Append(qq, Head(ws)), u + Head(ws).w, o, Tail(ws))
    ELSE <<qq, u, ws>>

\* Fault FirstFit: admit the first waiter that fits, wherever it is.
FitIdx(u, o, ws) == {k \in DOMAIN ws : Fits(u, o, ws[k].w)}
RECURSIVE FillAny(_, _, _, _)
FillAny(qq, u, o, ws) ==
    IF FitIdx(u, o, ws) # {}
    THEN LET k == CHOOSE k \in FitIdx(u, o, ws) : \A j \in FitIdx(u, o, ws) : k <= j
         IN FillAny(Append(qq, ws[k]), u + ws[k].w, o, Remove(ws, k))
    ELSE <<qq, u, ws>>

\* Credits were released (or a waiter left): hand off, in the same step.
\* Sets q, used, waiters, out (from out0), admitted and overtaken.
Handoff(qq, u, o, ws, out0) ==
    LET r == CASE Fault = "LazyHandoff" -> <<qq, u, ws>>
               [] Fault = "FirstFit" -> FillAny(qq, u, o, ws)
               [] OTHER -> Fill(qq, u, o, ws)
        adm == SeqIds(ws) \ SeqIds(r[3])
        passed == {ws[j].id : j \in {j \in DOMAIN ws :
                      /\ ws[j].id \in SeqIds(r[3])
                      /\ \E k \in DOMAIN ws : k > j /\ ws[k].id \in adm}}
    IN /\ q' = r[1] /\ used' = r[2] /\ waiters' = r[3]
       /\ out' = [i \in Ids |-> IF i \in adm THEN Accepted(i[1]) ELSE out0[i]]
       /\ admitted' = admitted \cup adm
       /\ overtaken' = overtaken \cup passed

-----------------------------------------------------------------------------
\* Senders

Begin(s) ==
    /\ Free(s)
    /\ LET n == sent[s] + 1
           x == Item(s, n)
           dead == ph = "dead" /\ Fault # "LateAdmit"
           big == Weight[s] > Limit /\ Fault # "OversizeWaits"
           direct == /\ (waiters = <<>> \/ Fault = "Barge")
                     /\ (Fits(used, ow, x.w) \/ Fault = "NoCapacityCheck")
       IN /\ sent' = [sent EXCEPT ![s] = n]
          /\ CASE dead ->
                    /\ out' = [out EXCEPT ![x.id] = "noproc"]
                    /\ UNCHANGED <<q, used, waiters, admitted, overtaken>>
               [] big ->
                    /\ out' = [out EXCEPT ![x.id] = "toolarge"]
                    /\ UNCHANGED <<q, used, waiters, admitted, overtaken>>
               [] direct ->
                    /\ q' = Append(q, x) /\ used' = used + x.w
                    /\ out' = [out EXCEPT ![x.id] = Accepted(s)]
                    /\ admitted' = admitted \cup {x.id}
                    /\ overtaken' = overtaken \cup SeqIds(waiters)
                    /\ UNCHANGED waiters
               [] Mode[s] = "try" ->
                    /\ out' = [out EXCEPT ![x.id] = "full"]
                    /\ UNCHANGED <<q, used, waiters, admitted, overtaken>>
               [] OTHER ->
                    /\ waiters' = Append(waiters, x)
                    /\ UNCHANGED <<q, used, out, admitted, overtaken>>
    /\ UNCHANGED <<ph, cur, cq, ow, handled, skipped, arrived, overtake, lost>>

\* A waiting sender's deadline passes: it leaves the queue never admitted
\* (MailboxFull), atomically with the hand-off.
Expire(s) ==
    /\ ph # "dead" /\ sent[s] > 0
    /\ LET i == <<s, sent[s]>> IN
       \/ /\ InWaiters(i)
          /\ LET k == CHOOSE k \in DOMAIN waiters : waiters[k].id = i
                 why == IF Fault = "TimeoutWhileWaiting" /\ Kind[s] = "req"
                        THEN "timeout" ELSE "full"
             IN Handoff(q, used, ow, Remove(waiters, k), [out EXCEPT ![i] = why])
       \/ /\ Fault = "ExpireRacesHandoff"
          /\ Mode[s] = "wait" /\ i \in SeqIds(q)
          /\ out' = [out EXCEPT ![i] = "full"]
          /\ UNCHANGED <<q, used, waiters, admitted, overtaken>>
    /\ UNCHANGED <<ph, cur, cq, ow, sent, handled, skipped, arrived, overtake, lost>>

\* An admitted request's caller deadline passes (R3): RequestTimeout.
CallerTimeout(s) ==
    /\ sent[s] > 0 /\ out[<<s, sent[s]>>] = "admitted"
    /\ out' = [out EXCEPT ![<<s, sent[s]>>] = "timeout"]
    /\ UNCHANGED <<ph, cur, q, cq, waiters, used, ow, sent, handled, skipped,
                   arrived, overtake, admitted, lost, overtaken>>

-----------------------------------------------------------------------------
\* Framework notices: one per edge, no credit, never refused.

CtlArrive(e) ==
    /\ e \notin arrived /\ ph # "dead"
    /\ arrived' = arrived \cup {e}
    /\ IF Fault = "CountControl" /\ ~Fits(used, ow, 1)
       THEN /\ lost' = lost \cup {e}
            /\ UNCHANGED <<q, cq>>
       ELSE IF e \in Prio
            THEN /\ cq' = Append(cq, Ctl(e))
                 /\ UNCHANGED <<q, lost>>
            ELSE /\ q' = Append(q, Ctl(e))
                 /\ UNCHANGED <<cq, lost>>
    /\ UNCHANGED <<ph, cur, waiters, used, ow, sent, out, handled, skipped,
                   overtake, admitted, overtaken>>

-----------------------------------------------------------------------------
\* The actor

PrioFirst ==
    /\ cq # <<>>
    /\ q = <<>> \/ overtake < Burst \/ Fault = "NoValve"

Dispatch ==
    /\ ph = "run" /\ cur = NoItem
    /\ \/ /\ PrioFirst
          /\ cur' = Head(cq) /\ cq' = Tail(cq)
          /\ handled' = handled \cup {Head(cq).id}
          /\ overtake' = IF q # <<>> THEN overtake + 1 ELSE 0
          /\ UNCHANGED <<q, used, waiters, out, skipped, admitted, overtaken>>
       \/ /\ q # <<>> /\ ~PrioFirst
          /\ LET x == Head(q)
                 skip == /\ x.kind = "req"
                         /\ Skip[x.src] \/ Fault = "SkipNoOptIn"
                         /\ \/ out[x.id] = "timeout"
                            \/ Fault = "SkipOpen" /\ out[x.id] = "admitted"
                 out1 == IF skip /\ out[x.id] = "admitted"
                         THEN [out EXCEPT ![x.id] = "dropped"] ELSE out
             IN /\ Handoff(Tail(q), used - x.w, ow, waiters, out1)
                /\ IF skip
                   THEN /\ skipped' = skipped \cup {x.id}
                        /\ cur' = NoItem
                        /\ UNCHANGED handled
                   ELSE /\ cur' = x
                        /\ handled' = handled \cup {x.id}
                        /\ UNCHANGED skipped
          /\ overtake' = 0
          /\ UNCHANGED cq
    /\ UNCHANGED <<ph, ow, sent, arrived, lost>>

TurnEnd ==
    /\ cur # NoItem
    /\ cur' = NoItem
    /\ out' = IF cur.kind = "req" /\ out[cur.id] = "admitted"
              THEN [out EXCEPT ![cur.id] = "ok"] ELSE out
    /\ UNCHANGED <<ph, q, cq, waiters, used, ow, sent, handled, skipped,
                   arrived, overtake, admitted, lost, overtaken>>

Stop ==
    /\ ph = "run"
    /\ ph' = "stopping"
    /\ UNCHANGED <<cur, q, cq, waiters, used, ow, sent, out, handled, skipped,
                   arrived, overtake, admitted, lost, overtaken>>

\* D: queued requests and request waiters are answered ActorStopped, send
\* waiters get NoProc, queues are discarded and the credits go back.
Death ==
    /\ ph = "stopping" /\ cur = NoItem
    /\ ph' = "dead"
    /\ q' = <<>> /\ cq' = <<>>
    /\ waiters' = IF Fault = "KeepWaiters" THEN waiters ELSE <<>>
    /\ used' = IF Fault = "LeakCredits" THEN used ELSE 0
    /\ out' = [i \in Ids |->
                 IF out[i] = "admitted" THEN "stopped"
                 ELSE IF InWaiters(i) /\ Fault # "KeepWaiters"
                      THEN (IF Kind[i[1]] = "req" THEN "stopped" ELSE "noproc")
                 ELSE out[i]]
    /\ UNCHANGED <<cur, ow, sent, handled, skipped, arrived, overtake,
                   admitted, lost, overtaken>>

-----------------------------------------------------------------------------
\* The other mailbox on a shared pool. It respects the pool's FIFO too.

OtherAcquire ==
    /\ ow < OtherMax /\ waiters = <<>> /\ Fits(used, ow, 1)
    /\ ow' = ow + 1
    /\ UNCHANGED <<ph, cur, q, cq, waiters, used, sent, out, handled, skipped,
                   arrived, overtake, admitted, lost, overtaken>>

OtherRelease ==
    /\ ow > 0
    /\ ow' = ow - 1
    /\ Handoff(q, used, ow - 1, waiters, out)
    /\ UNCHANGED <<ph, cur, cq, sent, handled, skipped, arrived, overtake, lost>>

-----------------------------------------------------------------------------

Next ==
    \/ \E s \in Senders : Begin(s) \/ Expire(s) \/ CallerTimeout(s)
    \/ \E e \in Edges : CtlArrive(e)
    \/ Dispatch \/ TurnEnd \/ Stop \/ Death
    \/ OtherAcquire \/ OtherRelease

Spec == Init /\ [][Next]_vars

\* The receiver drains, the other mailbox releases, a stopping actor reaches
\* D. Senders, deadlines and stops are not fair: a deadline may be Never.
LiveSpec == Spec /\ WF_vars(Dispatch) /\ WF_vars(TurnEnd) /\ WF_vars(Death)
                 /\ WF_vars(OtherRelease)

-----------------------------------------------------------------------------
\* Invariants

TypeOK ==
    /\ ph \in {"run", "stopping", "dead"}
    /\ used \in Nat /\ ow \in 0..OtherMax /\ overtake \in Nat
    /\ sent \in [Senders -> 0..MaxMsgs]
    /\ out \in [Ids -> Outcomes]
    /\ handled \subseteq Ids \cup CtlIds
    /\ skipped \subseteq Ids
    /\ arrived \subseteq Edges /\ lost \subseteq Edges
    /\ admitted \subseteq Ids /\ overtaken \subseteq Ids

\* A2: the credits held never exceed the pool, and match what is queued.
CapacityRespected ==
    /\ used + ow <= Limit
    /\ ph # "dead" => used = SumW(q)

\* A4: no free credits while the head waiter would fit (hand-off).
NoIdleWithWaiter ==
    (ph # "dead" /\ waiters # <<>>) => ~Fits(used, ow, Head(waiters).w)

\* A4: no waiter is passed over by a later sender, waiting or not.
NoOvertaking == overtaken = {}

\* A5: a message larger than the pool never waits.
NoOversizeWait == \A k \in DOMAIN waiters : waiters[k].w <= Limit

\* A3, A6: a rejection means never admitted, never handled.
RejectedNeverAdmitted ==
    \A i \in Ids : out[i] \in {"full", "toolarge", "noproc"} =>
        i \notin admitted \cup handled \cup skipped

\* A6: RequestTimeout (and every later outcome of a request) only after
\* admission; a request never admitted reports MailboxFull instead.
TimeoutOnlyAdmitted ==
    \A i \in Ids : out[i] \in {"timeout", "ok", "dropped"} => i \in admitted

\* A7: nothing is admitted past D (D3).
NoAdmitAfterD == ph = "dead" => q = <<>> /\ cq = <<>>

\* A7: every waiter is released at D.
NoWaiterAfterD == ph = "dead" => waiters = <<>>

\* A7: a dead instance holds no credits of a (possibly shared) pool.
CreditsReturnedAtDeath == ph = "dead" => used = 0

\* A8: framework notices are never refused for capacity.
ControlNeverRefused == lost = {}

\* A9: only an opted-in handler skips, and only a request whose caller can
\* no longer be answered.
SkipOnlyClosed == \A i \in skipped : out[i] = "timeout" /\ Skip[i[1]]

\* A10: an ordinary item is overtaken by at most Burst priority turns.
BoundedOvertaking == overtake <= Burst

-----------------------------------------------------------------------------
\* Liveness (LiveSpec)

\* A4: a waiter is admitted, expires or is released at D. (It needs the
\* ordinary lane to drain past any priority items, so it also covers the
\* valve's progress; BoundedOvertaking is the valve's safety form.)
WaiterResolves == \A i \in Ids : InWaiters(i) ~> ~InWaiters(i)
=============================================================================
