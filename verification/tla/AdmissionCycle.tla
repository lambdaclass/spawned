--------------------------- MODULE AdmissionCycle ---------------------------
(***************************************************************************)
(* Cyclic sends between two bounded mailboxes (docs/design/admission.md,  *)
(* "Cycles"; rank 17's cyclic-send deadlock). Actors A and B each have a  *)
(* mailbox of Cap slots. Outside senders fill both. Handling an outside   *)
(* message makes the handler send one note to the other actor; handling a *)
(* note sends nothing.                                                    *)
(*                                                                         *)
(* HandlerSend = "try": the handler's `send` fails at once on a full      *)
(* mailbox (MailboxFull). HandlerSend = "wait": it waits for a slot under *)
(* a finite deadline, so it eventually gives up if no slot frees. Either  *)
(* way every turn ends (TurnsEnd).                                        *)
(*                                                                         *)
(* Fault "NoDeadline" (negative control): the handler waits with          *)
(* Deadline::Never. Both actors can then wait on each other's full        *)
(* mailbox forever and TurnsEnd fails: the deadlock the design documents  *)
(* instead of preventing (semantics §10, rank 7).                         *)
(***************************************************************************)
EXTENDS Naturals, Sequences

CONSTANTS Cap, Inj, HandlerSend, Fault

Actors == {"A", "B"}
Other(a) == IF a = "A" THEN "B" ELSE "A"

VARIABLES q, st, inj
vars == <<q, st, inj>>

Init ==
    /\ q = [a \in Actors |-> <<>>]
    /\ st = [a \in Actors |-> "idle"]
    /\ inj = [a \in Actors |-> Inj]

\* An outside sender's message is admitted when a slot is free (an outside
\* waiter's own wait does not matter here).
Inject(a) ==
    /\ inj[a] > 0 /\ Len(q[a]) < Cap
    /\ q' = [q EXCEPT ![a] = Append(@, "ext")]
    /\ inj' = [inj EXCEPT ![a] = @ - 1]
    /\ UNCHANGED st

Dispatch(a) ==
    /\ st[a] = "idle" /\ q[a] # <<>>
    /\ q' = [q EXCEPT ![a] = Tail(@)]
    /\ st' = [st EXCEPT ![a] = IF Head(q[a]) = "ext" THEN "sending" ELSE "idle"]
    /\ UNCHANGED inj

\* The handler's send is admitted; its turn ends.
Deliver(a) ==
    /\ st[a] = "sending" /\ Len(q[Other(a)]) < Cap
    /\ q' = [q EXCEPT ![Other(a)] = Append(@, "note")]
    /\ st' = [st EXCEPT ![a] = "idle"]
    /\ UNCHANGED inj

\* The send fails (try on a full mailbox) or its deadline passes (wait);
\* the handler handles the MailboxFull error and its turn ends.
GiveUp(a) ==
    /\ st[a] = "sending"
    /\ \/ HandlerSend = "try" /\ Len(q[Other(a)]) >= Cap
       \/ HandlerSend = "wait" /\ Fault # "NoDeadline"
    /\ st' = [st EXCEPT ![a] = "idle"]
    /\ UNCHANGED <<q, inj>>

Next == \E a \in Actors : Inject(a) \/ Dispatch(a) \/ Deliver(a) \/ GiveUp(a)

\* Actors run; a finite deadline passes. Outside senders are not fair.
Spec == Init /\ [][Next]_vars
        /\ \A a \in Actors : WF_vars(Dispatch(a)) /\ WF_vars(Deliver(a) \/ GiveUp(a))

TypeOK ==
    /\ q \in [Actors -> Seq({"ext", "note"})]
    /\ \A a \in Actors : Len(q[a]) <= Cap
    /\ st \in [Actors -> {"idle", "sending"}]
    /\ inj \in [Actors -> 0..Inj]

\* A handler's send never blocks its turn forever.
TurnsEnd == \A a \in Actors : (st[a] = "sending") ~> (st[a] = "idle")
=============================================================================
