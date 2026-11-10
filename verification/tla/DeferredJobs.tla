---------------------------- MODULE DeferredJobs ----------------------------
(***************************************************************************)
(* Rank 11 design model, part 2: pipe-to-self jobs. Design contract:      *)
(* docs/design/deferred-replies.md. Part 1 is DeferredReplies.tla.        *)
(*                                                                         *)
(* An owner slot is restarted up to MaxGen times; each instance has a new *)
(* ActorId. Jobs are unlinked (ctx.pipe) or keyed (ctx.pipe_keyed), with  *)
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
(***************************************************************************)
EXTENDS Naturals, FiniteSets, Sequences
CONSTANTS J, Keyed, JobBound, MaxGen, MaxEmit, Credits, Fault
ASSUME Fault \in {"None", "NoInstanceCheck", "NoCancelFence", "DoubleTerminal",
                  "ReleaseOnTimeout", "KeyNoCancel", "ReplacePendingOnly",
                  "EmitAfterTerminal", "NoEmitCredit"}

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
    seen        \* history of dispatched messages

vars == <<inst, alive, exec, owner, hasDl, cancelled, notif, emitted, inflight,
          started, mbox, termSent, seen>>

Init ==
    /\ inst = 1 /\ alive = TRUE
    /\ exec = [j \in Jobs |-> "none"] /\ owner = [j \in Jobs |-> 0]
    /\ hasDl = [j \in Jobs |-> FALSE]
    /\ cancelled = [j \in Jobs |-> FALSE] /\ notif = [j \in Jobs |-> "pending"]
    /\ emitted = [j \in Jobs |-> 0] /\ inflight = [j \in Jobs |-> 0]
    /\ started = <<>> /\ mbox = <<>>
    /\ termSent = [j \in Jobs |-> 0] /\ seen = {}

Running == {j \in Jobs : exec[j] = "running"}
Permits == IF Fault = "ReleaseOnTimeout" THEN {j \in Running : notif[j] = "pending"}
           ELSE Running
LiveKeyed == {j \in Keyed : owner[j] = inst /\ exec[j] # "none"
                            /\ ~cancelled[j] /\ notif[j] = "pending"}
\* Earlier keyed jobs a replacement must cancel.
ToReplace == IF Fault = "ReplacePendingOnly" THEN LiveKeyed
             ELSE {j \in Keyed : owner[j] = inst /\ exec[j] # "none" /\ ~cancelled[j]}

CancelSet(S) ==
    /\ cancelled' = [j \in Jobs |-> IF j \in S THEN TRUE ELSE cancelled[j]]
    /\ notif' = [j \in Jobs |-> IF j \in S /\ notif[j] = "pending" THEN "suppressed" ELSE notif[j]]

Start(j) ==
    /\ alive /\ exec[j] = "none" /\ Cardinality(Permits) < JobBound
    /\ exec' = [exec EXCEPT ![j] = "running"] /\ owner' = [owner EXCEPT ![j] = inst]
    /\ \E d \in BOOLEAN : hasDl' = [hasDl EXCEPT ![j] = d]
    /\ IF j \in Keyed
       THEN /\ started' = Append(started, j)
            /\ IF Fault = "KeyNoCancel" THEN UNCHANGED <<cancelled, notif>>
               ELSE CancelSet(ToReplace)
       ELSE UNCHANGED <<started, cancelled, notif>>
    /\ UNCHANGED <<inst, alive, emitted, inflight, mbox, termSent, seen>>

CanEmit(j) ==
    /\ exec[j] = "running" /\ emitted[j] < MaxEmit
    /\ (notif[j] = "pending" /\ ~cancelled[j]) \/ Fault = "EmitAfterTerminal"
    /\ inflight[j] < Credits \/ Fault = "NoEmitCredit"

Emit(j) ==
    /\ CanEmit(j)
    /\ emitted' = [emitted EXCEPT ![j] = @ + 1]
    /\ inflight' = [inflight EXCEPT ![j] = @ + 1]
    /\ mbox' = Append(mbox, [j |-> j, kind |-> "emit"])
    /\ UNCHANGED <<inst, alive, exec, owner, hasDl, cancelled, notif, started, termSent, seen>>

\* Physical completion; the notification is a one-shot.
Finish(j) ==
    /\ exec[j] = "running" /\ exec' = [exec EXCEPT ![j] = "finished"]
    /\ IF notif[j] = "pending" \/ (Fault = "DoubleTerminal" /\ notif[j] = "notified")
       THEN /\ notif' = [notif EXCEPT ![j] = "notified"]
            /\ mbox' = Append(mbox, [j |-> j, kind |-> "done"])
            /\ termSent' = [termSent EXCEPT ![j] = @ + 1]
       ELSE UNCHANGED <<notif, mbox, termSent>>
    /\ UNCHANGED <<inst, alive, owner, hasDl, cancelled, emitted, inflight, started, seen>>

\* Job deadline (only for jobs started with one): the notification is
\* DeadlineExceeded now; the work keeps running and keeps its permit.
JobDeadline(j) ==
    /\ hasDl[j] /\ exec[j] = "running" /\ notif[j] = "pending"
    /\ notif' = [notif EXCEPT ![j] = "notified"]
    /\ mbox' = Append(mbox, [j |-> j, kind |-> "deadline"])
    /\ termSent' = [termSent EXCEPT ![j] = @ + 1]
    /\ UNCHANGED <<inst, alive, exec, owner, hasDl, cancelled, emitted, inflight, started, seen>>

Cancel(j) ==
    /\ alive /\ owner[j] = inst /\ exec[j] # "none" /\ ~cancelled[j]
    /\ CancelSet({j})
    /\ UNCHANGED <<inst, alive, exec, owner, hasDl, emitted, inflight, started, mbox, termSent, seen>>

\* Owner exit cancels the live job set. Queued messages stay addressed to
\* the slot (the conservative case); they are fenced at dispatch.
Exit ==
    /\ alive /\ alive' = FALSE
    /\ CancelSet({j \in Jobs : owner[j] = inst /\ exec[j] = "running"})
    /\ UNCHANGED <<inst, exec, owner, hasDl, emitted, inflight, started, mbox, termSent, seen>>

Restart ==
    /\ ~alive /\ inst < MaxGen /\ inst' = inst + 1 /\ alive' = TRUE
    /\ UNCHANGED <<exec, owner, hasDl, cancelled, notif, emitted, inflight, started, mbox, termSent, seen>>

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
    /\ UNCHANGED <<inst, alive, exec, owner, hasDl, cancelled, notif, emitted, started, termSent>>

Next ==
    \/ Exit \/ Restart \/ Dispatch
    \/ \E j \in Jobs : Start(j) \/ Emit(j) \/ Finish(j) \/ JobDeadline(j) \/ Cancel(j)

Spec == Init /\ [][Next]_vars
FairSpec == Spec /\ (\A j \in Jobs : WF_vars(JobDeadline(j))) /\ WF_vars(Dispatch)

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
OneLivePerKey == Cardinality(LiveKeyed) <= 1
\* A job's terminal message is the last message it puts in the mailbox.
TerminalLast ==
    \A a, b \in 1..Len(mbox) :
        (a < b /\ mbox[a].j = mbox[b].j /\ IsTerm(mbox[a])) => FALSE
EmitCreditsBounded == \A j \in Jobs : inflight[j] <= Credits

Safety ==
    /\ TypeOK /\ NoLateResult /\ NoStaleAfterCancel /\ NoStaleKeyed
    /\ AtMostOneTerminal /\ PermitsBounded /\ OneLivePerKey
    /\ TerminalLast /\ EmitCreditsBounded

\* A job started with a deadline and never cancelled is reported to its
\* live owner instance even if its work never finishes.
JobReported ==
    \A j \in Jobs :
        (hasDl[j] /\ exec[j] # "none" /\ ~cancelled[j] /\ alive /\ owner[j] = inst)
            ~> (cancelled[j] \/ TermSeen(j) # {} \/ ~alive \/ owner[j] # inst)
=============================================================================
