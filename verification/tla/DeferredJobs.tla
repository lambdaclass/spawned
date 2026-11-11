---------------------------- MODULE DeferredJobs ----------------------------
(***************************************************************************)
(* Rank 11 design model, part 2: pipe-to-self jobs. Design contract:      *)
(* docs/design/deferred-replies.md. Part 1 is DeferredReplies.tla.        *)
(*                                                                         *)
(* An owner slot is restarted up to MaxGen times; each instance has a new *)
(* ActorId. Jobs are unlinked (ctx.pipe) or keyed (JobOptions::key), with *)
(* or without a job deadline. Each job has three separate states:         *)
(* execution (running/finished; cancellation is cooperative, so cancelled *)
(* or timed-out work keeps running and keeps its work permit),            *)
(* cancellation (a flag in the job's shared state) and terminal           *)
(* notification (pending -> notified once by completion, panic or the     *)
(* deadline; -> suppressed by cancellation or owner exit).               *)
(*                                                                         *)
(* The mailbox is one FIFO sequence. A job emits only while its           *)
(* notification is pending and it is not cancelled (the emit check and   *)
(* the terminal enqueue are serialized), and only with an in-flight emit  *)
(* credit; a credit returns when its message is dispatched or fenced. The *)
(* terminal message needs no credit. A new keyed job cancels every        *)
(* earlier job for the key that is not yet cancelled, including notified  *)
(* and finished ones, so their queued messages are fenced.               *)
(*                                                                         *)
(* Messages are addressed to the slot (a durable address can reach a      *)
(* newer instance). Dispatch drops a message whose job belongs to another *)
(* ActorId or is cancelled, reading the job's shared state.               *)
(*                                                                         *)
(* Fault selects a negative control; negative_checks.py requires each to  *)
(* violate the named invariant:                                           *)
(*   NoInstanceCheck   (NoLateResult)                                      *)
(*   NoCancelFence     (NoStaleAfterCancel)                                *)
(*   DoubleTerminal    (AtMostOneTerminal)                                 *)
(*   ReleaseOnTimeout  (PermitsBounded)                                    *)
(*   KeyNoCancel       (OneLivePerKey)                                     *)
(*   ReplacePendingOnly(NoStaleKeyed)  replacement skips notified jobs     *)
(*   EmitAfterTerminal (TerminalLast)  emits continue after the terminal   *)
(*   NoEmitCredit      (EmitCreditsBounded)                                *)
(*   EarlyCredit       (HeldCounted)  a held result returns its credit    *)
(*                     when the work ends                                  *)
(*   HeldDeadlineLost  (DeadlineArmedWhileHeld)  the deadline cannot take  *)
(*                     a held result's notification                        *)
(*   ExitKeepsHeld     (ExitCancelsJobs)  owner exit cancels running jobs  *)
(*                     only, not held results or scheduled emits           *)
(*   NoAdmissionFence  (ExitCancelsJobs)  a job is admitted after the exit *)
(*                     cancelled the job set (admission and cancel_all are *)
(*                     not serialized under the jobs lock)                 *)
(*                                                                         *)
(* Delayed emits (emit_at) and held results: a running job may schedule an *)
(* emit (it takes an emit credit at once and is enqueued, fenced like any  *)
(* emit, when it fires). When the work ends while emits are scheduled, its *)
(* result is held: the terminal message is enqueued after the last         *)
(* scheduled emit fires, so it stays last. Each job owns one              *)
(* MAX_OUTSTANDING credit (`credit`) from admission until its terminal     *)
(* message is enqueued, or, when the notification was taken or suppressed  *)
(* otherwise, until execution ends: a held result keeps it (HeldCounted).  *)
(* The job deadline can take a held result's notification (it stays armed *)
(* until the terminal message).                                            *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets, Sequences
CONSTANTS J, Keyed, JobBound, MaxGen, MaxEmit, Credits, Fault
ASSUME Fault \in {"None", "NoInstanceCheck", "NoCancelFence", "DoubleTerminal",
                  "ReleaseOnTimeout", "KeyNoCancel", "ReplacePendingOnly",
                  "EmitAfterTerminal", "NoEmitCredit", "EarlyCredit",
                  "HeldDeadlineLost", "ExitKeepsHeld", "NoAdmissionFence"}

Jobs == 1..J
ASSUME Keyed \subseteq Jobs

VARIABLES
    inst, alive,
    exec,       \* none, running, finished
    owner,      \* ActorId that started the job
    hasDl,      \* the job was started with a deadline
    cancelled, notif,
    emitted,    \* lifetime emits (bounds the state space)
    inflight,   \* emits enqueued and not yet dispatched or fenced
    started,    \* order in which keyed jobs started (for NoStaleKeyed)
    mbox,       \* FIFO sequence of [j, kind]
    termSent,   \* terminal messages enqueued per job
    seen,       \* history of dispatched messages
    sched,      \* scheduled (emit_at) emits not yet fired, each with a credit
    held,       \* the work ended with emits scheduled: its result is held
    credit      \* the job holds its MAX_OUTSTANDING credit

vars == <<inst, alive, exec, owner, hasDl, cancelled, notif, emitted, inflight,
          started, mbox, termSent, seen, sched, held, credit>>

\* Variables of delayed emits and held results, unchanged by the steps that
\* do not concern them.
heldVars == <<sched, held, credit>>

Init ==
    /\ inst = 1 /\ alive = TRUE
    /\ exec = [j \in Jobs |-> "none"] /\ owner = [j \in Jobs |-> 0]
    /\ hasDl = [j \in Jobs |-> FALSE]
    /\ cancelled = [j \in Jobs |-> FALSE] /\ notif = [j \in Jobs |-> "pending"]
    /\ emitted = [j \in Jobs |-> 0] /\ inflight = [j \in Jobs |-> 0]
    /\ started = <<>> /\ mbox = <<>>
    /\ termSent = [j \in Jobs |-> 0] /\ seen = {}
    /\ sched = [j \in Jobs |-> 0] /\ held = [j \in Jobs |-> FALSE]
    /\ credit = [j \in Jobs |-> FALSE]

Running == {j \in Jobs : exec[j] = "running"}
\* Jobs holding a MAX_OUTSTANDING credit (admission checks the bound).
Permits == {j \in Jobs : credit[j]}
LiveKeyed == {j \in Keyed : owner[j] = inst /\ exec[j] # "none"
                            /\ ~cancelled[j] /\ notif[j] = "pending"}
\* Earlier keyed jobs a replacement must cancel.
ToReplace == IF Fault = "ReplacePendingOnly" THEN LiveKeyed
             ELSE {j \in Keyed : owner[j] = inst /\ exec[j] # "none" /\ ~cancelled[j]}

\* Cancellation drops scheduled emits (their credits return) and a held
\* result, whose run then releases the job's credit.
CancelCore(S) ==
    /\ cancelled' = [j \in Jobs |-> IF j \in S THEN TRUE ELSE cancelled[j]]
    /\ notif' = [j \in Jobs |-> IF j \in S /\ notif[j] = "pending" THEN "suppressed" ELSE notif[j]]
    /\ sched' = [j \in Jobs |-> IF j \in S THEN 0 ELSE sched[j]]
    /\ held' = [j \in Jobs |-> IF j \in S THEN FALSE ELSE held[j]]
CreditAfterCancel(S) ==
    [j \in Jobs |-> IF j \in S /\ exec[j] = "finished" THEN FALSE ELSE credit[j]]
CancelSet(S) == CancelCore(S) /\ credit' = CreditAfterCancel(S)

\* Admission and the exit's cancel_all are serialized (one jobs lock and
\* its closed flag): no job is admitted once the exit cancelled the set.
Start(j) ==
    /\ alive \/ Fault = "NoAdmissionFence"
    /\ exec[j] = "none" /\ Cardinality(Permits) < JobBound
    /\ exec' = [exec EXCEPT ![j] = "running"] /\ owner' = [owner EXCEPT ![j] = inst]
    /\ \E d \in BOOLEAN : hasDl' = [hasDl EXCEPT ![j] = d]
    /\ IF j \in Keyed /\ Fault # "KeyNoCancel"
       THEN /\ started' = Append(started, j)
            /\ CancelCore(ToReplace)
            /\ credit' = [CreditAfterCancel(ToReplace) EXCEPT ![j] = TRUE]
       ELSE /\ started' = IF j \in Keyed THEN Append(started, j) ELSE started
            /\ credit' = [credit EXCEPT ![j] = TRUE]
            /\ UNCHANGED <<cancelled, notif, sched, held>>
    /\ UNCHANGED <<inst, alive, emitted, inflight, mbox, termSent, seen>>

Accepting(j) == notif[j] = "pending" /\ ~cancelled[j]

CanEmit(j) ==
    /\ exec[j] = "running" /\ emitted[j] < MaxEmit
    /\ Accepting(j) \/ Fault = "EmitAfterTerminal"
    /\ inflight[j] + sched[j] < Credits \/ Fault = "NoEmitCredit"

Emit(j) ==
    /\ CanEmit(j)
    /\ emitted' = [emitted EXCEPT ![j] = @ + 1]
    /\ inflight' = [inflight EXCEPT ![j] = @ + 1]
    /\ mbox' = Append(mbox, [j |-> j, kind |-> "emit"])
    /\ UNCHANGED <<inst, alive, exec, owner, hasDl, cancelled, notif, started, termSent, seen,
                   heldVars>>

\* emit_at: take an emit credit now; the emit is enqueued when it fires.
ScheduleEmit(j) ==
    /\ exec[j] = "running" /\ emitted[j] < MaxEmit /\ Accepting(j)
    /\ inflight[j] + sched[j] < Credits
    /\ emitted' = [emitted EXCEPT ![j] = @ + 1]
    /\ sched' = [sched EXCEPT ![j] = @ + 1]
    /\ UNCHANGED <<inst, alive, exec, owner, hasDl, cancelled, notif, inflight, started, mbox,
                   termSent, seen, held, credit>>

\* A scheduled emit fires: enqueued while the notification is pending (else
\* dropped, its credit returning). The last one releases a held result: its
\* terminal message, then the job's credit.
FireScheduled(j) ==
    /\ sched[j] > 0
    /\ sched' = [sched EXCEPT ![j] = @ - 1]
    /\ LET fires == Accepting(j)
           release == held[j] /\ sched[j] = 1
           emitMsg == IF fires THEN <<[j |-> j, kind |-> "emit"]>> ELSE <<>>
           doneMsg == IF release THEN <<[j |-> j, kind |-> "done"]>> ELSE <<>>
       IN /\ mbox' = mbox \o emitMsg \o doneMsg
          /\ inflight' = IF fires THEN [inflight EXCEPT ![j] = @ + 1] ELSE inflight
          /\ IF release
             THEN /\ held' = [held EXCEPT ![j] = FALSE]
                  /\ notif' = [notif EXCEPT ![j] = "notified"]
                  /\ termSent' = [termSent EXCEPT ![j] = @ + 1]
                  /\ credit' = [credit EXCEPT ![j] = FALSE]
             ELSE UNCHANGED <<held, notif, termSent, credit>>
    /\ UNCHANGED <<inst, alive, exec, owner, hasDl, cancelled, emitted, started, seen>>

\* Physical completion; the notification is a one-shot. With emits still
\* scheduled the result is held (keeping the credit); otherwise the terminal
\* message is enqueued, or nothing when the notification was already taken
\* or suppressed, and the credit returns.
Finish(j) ==
    /\ exec[j] = "running" /\ exec' = [exec EXCEPT ![j] = "finished"]
    /\ IF (Accepting(j) /\ sched[j] = 0)
          \/ (Fault = "DoubleTerminal" /\ notif[j] = "notified")
       THEN /\ notif' = [notif EXCEPT ![j] = "notified"]
            /\ mbox' = Append(mbox, [j |-> j, kind |-> "done"])
            /\ termSent' = [termSent EXCEPT ![j] = @ + 1]
            /\ credit' = [credit EXCEPT ![j] = FALSE]
            /\ UNCHANGED held
       ELSE IF Accepting(j)
       THEN /\ held' = [held EXCEPT ![j] = TRUE]
            /\ credit' = IF Fault = "EarlyCredit" THEN [credit EXCEPT ![j] = FALSE]
                          ELSE credit
            /\ UNCHANGED <<notif, mbox, termSent>>
       ELSE /\ credit' = [credit EXCEPT ![j] = FALSE]
            /\ UNCHANGED <<notif, mbox, termSent, held>>
    /\ UNCHANGED <<inst, alive, owner, hasDl, cancelled, emitted, inflight, started, seen, sched>>

\* Job deadline (only for jobs started with one): the notification is
\* DeadlineExceeded now. Running work keeps running and keeps its credit; a
\* held result is dropped and its credit returns.
JobDeadline(j) ==
    /\ hasDl[j] /\ notif[j] = "pending"
    /\ exec[j] = "running" \/ (held[j] /\ Fault # "HeldDeadlineLost")
    /\ notif' = [notif EXCEPT ![j] = "notified"]
    /\ mbox' = Append(mbox, [j |-> j, kind |-> "deadline"])
    /\ termSent' = [termSent EXCEPT ![j] = @ + 1]
    /\ held' = [held EXCEPT ![j] = FALSE]
    /\ credit' = IF held[j] \/ Fault = "ReleaseOnTimeout"
                  THEN [credit EXCEPT ![j] = FALSE] ELSE credit
    /\ UNCHANGED <<inst, alive, exec, owner, hasDl, cancelled, emitted, inflight, started, seen,
                   sched>>

Cancel(j) ==
    /\ alive /\ owner[j] = inst /\ exec[j] # "none" /\ ~cancelled[j]
    /\ CancelSet({j})
    /\ UNCHANGED <<inst, alive, exec, owner, hasDl, emitted, inflight, started, mbox, termSent, seen>>

\* Owner exit cancels the live job set (running, or with emits scheduled or
\* a result held). Queued messages stay addressed to the slot (the
\* conservative case); they are fenced at dispatch.
Exit ==
    /\ alive /\ alive' = FALSE
    /\ CancelSet({j \in Jobs : owner[j] = inst
                               /\ (exec[j] = "running"
                                   \/ ((sched[j] > 0 \/ held[j]) /\ Fault # "ExitKeepsHeld"))})
    /\ UNCHANGED <<inst, exec, owner, hasDl, emitted, inflight, started, mbox, termSent, seen>>

Restart ==
    /\ ~alive /\ inst < MaxGen /\ inst' = inst + 1 /\ alive' = TRUE
    /\ UNCHANGED <<exec, owner, hasDl, cancelled, notif, emitted, inflight, started, mbox, termSent, seen,
                   heldVars>>

Fenced(m) ==
    \/ owner[m.j] # inst /\ Fault # "NoInstanceCheck"
    \/ cancelled[m.j] /\ Fault # "NoCancelFence"

\* Position of a keyed job in the start order (0 if unkeyed).
Pos(j) == IF \E i \in 1..Len(started) : started[i] = j
          THEN CHOOSE i \in 1..Len(started) : started[i] = j ELSE 0

Dispatch ==
    /\ alive /\ mbox # <<>>
    /\ LET m == Head(mbox) IN
       /\ mbox' = Tail(mbox)
       /\ inflight' = IF m.kind = "emit" THEN [inflight EXCEPT ![m.j] = @ - 1] ELSE inflight
       /\ IF Fenced(m) THEN UNCHANGED seen
          ELSE seen' = seen \cup {[j |-> m.j, kind |-> m.kind, owner |-> owner[m.j],
                                   at |-> inst, cancelled |-> cancelled[m.j],
                                   superseded |-> m.j \in Keyed /\ Pos(m.j) < Len(started)]}
    /\ UNCHANGED <<inst, alive, exec, owner, hasDl, cancelled, notif, emitted, started, termSent,
                   heldVars>>

Next ==
    \/ Exit \/ Restart \/ Dispatch
    \/ \E j \in Jobs : Start(j) \/ Emit(j) \/ Finish(j) \/ JobDeadline(j) \/ Cancel(j)
                      \/ ScheduleEmit(j) \/ FireScheduled(j)

Spec == Init /\ [][Next]_vars
FairSpec == Spec /\ (\A j \in Jobs : WF_vars(JobDeadline(j)) /\ WF_vars(FireScheduled(j)))
            /\ WF_vars(Dispatch)

------------------------------------------------------------------------------
\* Properties

TypeOK ==
    /\ inst \in 1..MaxGen /\ alive \in BOOLEAN
    /\ exec \in [Jobs -> {"none", "running", "finished"}]
    /\ notif \in [Jobs -> {"pending", "notified", "suppressed"}]

TermSeen(j) == {s \in seen : s.j = j /\ s.kind \in {"done", "deadline"}}
IsTerm(m) == m.kind \in {"done", "deadline"}

NoLateResult == \A s \in seen : s.owner = s.at
NoStaleAfterCancel == \A s \in seen : ~s.cancelled
\* No message of a keyed job is dispatched after a newer keyed job started.
NoStaleKeyed == \A s \in seen : ~s.superseded
AtMostOneTerminal == \A j \in Jobs : termSent[j] <= 1 /\ Cardinality(TermSeen(j)) <= 1
PermitsBounded == Cardinality(Running) <= JobBound
\* Running work and held results count against the bound: a held result
\* keeps the job's credit until its terminal message.
HeldCounted == Cardinality({j \in Jobs : exec[j] = "running" \/ held[j]}) <= JobBound
\* The job deadline stays armed while a result is held.
DeadlineArmedWhileHeld ==
    \A j \in Jobs : (held[j] /\ hasDl[j] /\ notif[j] = "pending") => ENABLED JobDeadline(j)
OneLivePerKey == Cardinality(LiveKeyed) <= 1
\* A job's terminal message is the last message it puts in the mailbox.
TerminalLast ==
    \A a, b \in 1..Len(mbox) :
        (a < b /\ mbox[a].j = mbox[b].j /\ IsTerm(mbox[a])) => FALSE
EmitCreditsBounded == \A j \in Jobs : inflight[j] + sched[j] <= Credits
\* After the owner's exit nothing of its instance is live: running work,
\* scheduled emits and held results are all cancelled (their notification
\* suppressed, held results dropped).
ExitCancelsJobs ==
    ~alive => \A j \in Jobs :
        (owner[j] = inst /\ (exec[j] = "running" \/ sched[j] > 0 \/ held[j])) => cancelled[j]

Safety ==
    /\ TypeOK /\ NoLateResult /\ NoStaleAfterCancel /\ NoStaleKeyed
    /\ AtMostOneTerminal /\ PermitsBounded /\ OneLivePerKey
    /\ TerminalLast /\ EmitCreditsBounded /\ HeldCounted /\ DeadlineArmedWhileHeld
    /\ ExitCancelsJobs

\* A job started with a deadline and never cancelled is reported to its
\* live owner instance even if its work never finishes.
JobReported ==
    \A j \in Jobs :
        (hasDl[j] /\ exec[j] # "none" /\ ~cancelled[j] /\ alive /\ owner[j] = inst)
            ~> (cancelled[j] \/ TermSeen(j) # {} \/ ~alive \/ owner[j] # inst)
=============================================================================
