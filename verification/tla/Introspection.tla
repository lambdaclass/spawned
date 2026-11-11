--------------------------- MODULE Introspection ---------------------------
(***************************************************************************)
(* Introspection v1 design model, revision 2 (docs/design/introspection.md, *)
(* not implemented). One actor and its activity word, an in-band status    *)
(* request (sys:get_status) and the stall watchdog's fenced kill.          *)
(*                                                                         *)
(* The activity word is (turn id, open bit, kill bit). Every activity      *)
(* opens a new turn id: started(), each message, each status request and   *)
(* stopped(). The run loop closes it with a swap that reads the kill bit;  *)
(* the watchdog commits a kill with one CAS on (observed id, open). A      *)
(* failed CAS has no effect. Every way an activity ends consumes the kill  *)
(* bit: normal end, handler panic, status callback panic, hook end and     *)
(* runtime drop. The death point D closes the word for good.               *)
(*                                                                         *)
(* Out-of-band reads (process_info, listings, the watchdog's sampling) are *)
(* not actions: they change nothing.                                       *)
(*                                                                         *)
(* Fault = "None" or a negative control that must fail (negative_checks.py *)
(* INTROSPECTION_CONTROLS, each on its named invariant):                   *)
(*   CheckThenAct        watchdog checks the turn, kills in a later step   *)
(*   FailedCasKills      a failed CAS still records a kill                 *)
(*   KillBitIgnored      normal turn end drops the kill bit                *)
(*   PanicOverKill       a handler panic ignores the kill bit              *)
(*   DropLosesKill       runtime drop ignores the kill bit                 *)
(*   NoInvalidateAtD     runtime drop leaves the word open after D         *)
(*   KillRunsStopped     stopped() begins after a kill                     *)
(*   DispatchAfterStop   the run loop dispatches after a stop or kill      *)
(*   StatusInTurn        status answered while a turn is suspended         *)
(*   StatusLost          a waiting status request is not answered at D     *)
(*   DoubleAnswer        a status request answered twice                   *)
(*   CallbackAfterGiveUp the callback runs after its caller timed out      *)
(*   CallbackPanicStops  a caught callback panic ends the actor            *)
(***************************************************************************)
EXTENDS Naturals, Sequences, FiniteSets

CONSTANTS
    MaxTurns,   \* messages sent
    Fault

None == "none"
MaxSeq == MaxTurns + 3   \* started(), one status turn, stopped()

VARIABLES
    ph,        \* "start", "idle", "turn", "hook", "exiting", "dead" (past D), "gone" (published)
    kind,      \* open activity: "start", "msg", "status", "hook", or None
    seq,       \* activity word: turn id of the current or last activity
    open,      \* activity word: an activity is open
    kbit,      \* activity word: kill bit (set only by the watchdog's CAS)
    susp,      \* a message turn is suspended at an await (may never resume)
    killed,    \* a kill is recorded in the exit request
    stopReq,   \* an administrative stop (Shutdown) is recorded
    panicked,  \* a handler panic is recorded
    reason,    \* final reason, fixed at D
    mbox,      \* queued items: "msg" or "status"
    sent,
    st,        \* the status caller: "unsent", "waiting", "ok", "failed", "timeout", "stopped", "noproc"
    answers,   \* answers produced for the status request (by its turn or at D)
    stSkip,    \* the open status turn's caller had given up at dispatch
    wdPhase,   \* watchdog: "idle", "committed" (CAS succeeded), "decided" (bug paths), "done"
    obs,       \* watchdog: observed open turn id, or 0
    wdTarget,  \* watchdog: turn id its kill targets
    \* ghosts
    endedClean,     \* turn ids whose activity ended with no kill bit set
    charged,        \* a watchdog kill was recorded for a turn that had ended clean
    commitPreD,     \* a CAS succeeded before D
    commitPostD,    \* a CAS succeeded after D
    lateDispatch,   \* something dispatched after a stop, a kill or a committed CAS
    hookAfterKill,  \* stopped() began although a kill was recorded
    stInTurn,       \* status answered while another turn was open
    cbAfterGiveUp,  \* a status callback ran after its caller timed out
    cbStopped       \* a caught status callback panic began exit

vars == <<ph, kind, seq, open, kbit, susp, killed, stopReq, panicked, reason,
          mbox, sent, st, answers, stSkip, wdPhase, obs, wdTarget, endedClean,
          charged, commitPreD, commitPostD, lateDispatch, hookAfterKill,
          stInTurn, cbAfterGiveUp, cbStopped>>

actorVars == <<ph, kind, seq, open, kbit, susp, killed, stopReq, panicked, reason>>
ghostVars == <<endedClean, charged, commitPreD, commitPostD, lateDispatch,
               hookAfterKill, stInTurn, cbAfterGiveUp, cbStopped>>
wdVars    == <<wdPhase, obs, wdTarget>>
stVars    == <<st, answers, stSkip>>

Init ==
    /\ ph = "start" /\ kind = "start" /\ seq = 1 /\ open = TRUE /\ kbit = FALSE
    /\ susp = FALSE /\ killed = FALSE /\ stopReq = FALSE /\ panicked = FALSE
    /\ reason = None /\ mbox = <<>> /\ sent = 0
    /\ st = "unsent" /\ answers = 0 /\ stSkip = FALSE
    /\ wdPhase = "idle" /\ obs = 0 /\ wdTarget = 0
    /\ endedClean = {} /\ charged = FALSE /\ commitPreD = FALSE
    /\ commitPostD = FALSE /\ lateDispatch = FALSE /\ hookAfterKill = FALSE
    /\ stInTurn = FALSE /\ cbAfterGiveUp = FALSE /\ cbStopped = FALSE

Exiting == killed \/ stopReq \/ panicked
Past == ph \in {"dead", "gone"}
Committed == wdPhase = "committed" \/ (wdPhase = "done" /\ commitPreD)

\* Closing the activity word: the swap reads the kill bit.
Close(consume) ==
    /\ open' = FALSE /\ kbit' = FALSE /\ kind' = None
    /\ killed' = (killed \/ (kbit /\ consume))
    /\ endedClean' = IF kbit THEN endedClean ELSE endedClean \cup {seq}

FinalReason(k) ==
    IF k THEN "Killed" ELSE IF panicked THEN "Panic"
    ELSE IF stopReq THEN "Shutdown" ELSE "Normal"

\* At D every waiting status request is answered with the final reason.
AnswerAtDeath(queued) ==
    /\ st' = IF st = "waiting" /\ Fault # "StatusLost" THEN "stopped" ELSE st
    /\ answers' = answers + (IF (queued /\ Fault # "StatusLost") \/ Fault = "DoubleAnswer"
                             THEN 1 ELSE 0)

-----------------------------------------------------------------------------
\* The actor

StartedEnd ==
    /\ ph = "start"
    /\ ph' = "idle"
    /\ Close(TRUE)
    /\ UNCHANGED <<seq, susp, stopReq, panicked, reason, mbox, sent, stVars,
                   wdVars, charged, commitPreD, commitPostD, lateDispatch,
                   hookAfterKill, stInTurn, cbAfterGiveUp, cbStopped>>

Send ==
    /\ sent < MaxTurns
    /\ sent' = sent + 1
    /\ mbox' = IF Past THEN mbox ELSE Append(mbox, "msg")
    /\ UNCHANGED <<actorVars, stVars, wdVars, ghostVars>>

SendStatus ==
    /\ st = "unsent"
    /\ IF Past THEN /\ st' = "noproc" /\ mbox' = mbox
               ELSE /\ st' = "waiting" /\ mbox' = Append(mbox, "status")
    /\ UNCHANGED <<actorVars, sent, answers, stSkip, wdVars, ghostVars>>

\* The caller's deadline passes: it sees RequestTimeout; a later answer is
\* dropped (the slot is closed).
StatusTimeout ==
    /\ st = "waiting"
    /\ st' = "timeout"
    /\ UNCHANGED <<actorVars, mbox, sent, answers, stSkip, wdVars, ghostVars>>

\* The run loop checks for exit before every receive (D4). Every dispatch,
\* status included, opens a new turn id.
Dispatch ==
    /\ ph = "idle"
    /\ open = FALSE
    /\ ~Exiting \/ Fault = "DispatchAfterStop"
    /\ mbox # <<>>
    /\ mbox' = Tail(mbox)
    /\ ph' = "turn" /\ seq' = seq + 1 /\ open' = TRUE /\ susp' = FALSE
    /\ kind' = Head(mbox)
    /\ stSkip' = (Head(mbox) = "status" /\ st = "timeout")
    /\ lateDispatch' = (lateDispatch \/ Exiting \/ Committed)
    /\ UNCHANGED <<kbit, killed, stopReq, panicked, reason, sent, st, answers,
                   wdVars, endedClean, charged, commitPreD, commitPostD,
                   hookAfterKill, stInTurn, cbAfterGiveUp, cbStopped>>

Suspend ==
    /\ ph = "turn" /\ kind = "msg" /\ ~susp
    /\ susp' = TRUE
    /\ UNCHANGED <<ph, kind, seq, open, kbit, killed, stopReq, panicked, reason,
                   mbox, sent, stVars, wdVars, ghostVars>>

Resume ==
    /\ ph = "turn" /\ susp
    /\ susp' = FALSE
    /\ UNCHANGED <<ph, kind, seq, open, kbit, killed, stopReq, panicked, reason,
                   mbox, sent, stVars, wdVars, ghostVars>>

\* A message turn returns.
MsgEnd ==
    /\ ph = "turn" /\ kind = "msg" /\ ~susp
    /\ ph' = "idle"
    /\ Close(Fault # "KillBitIgnored")
    /\ UNCHANGED <<seq, susp, stopReq, panicked, reason, mbox, sent, stVars,
                   wdVars, charged, commitPreD, commitPostD, lateDispatch,
                   hookAfterKill, stInTurn, cbAfterGiveUp, cbStopped>>

\* A handler panics: the turn ends and exit begins; a committed kill wins.
MsgPanic ==
    /\ ph = "turn" /\ kind = "msg" /\ ~susp
    /\ ph' = "idle"
    /\ panicked' = TRUE
    /\ Close(Fault # "PanicOverKill")
    /\ UNCHANGED <<seq, susp, stopReq, reason, mbox, sent, stVars, wdVars,
                   charged, commitPreD, commitPostD, lateDispatch,
                   hookAfterKill, stInTurn, cbAfterGiveUp, cbStopped>>

\* The status turn: the callback runs unless the caller already gave up.
StatusEnd ==
    /\ ph = "turn" /\ kind = "status"
    /\ ph' = "idle"
    /\ \/ /\ stSkip /\ Fault # "CallbackAfterGiveUp"
          /\ UNCHANGED <<st, answers, cbAfterGiveUp>>
       \/ /\ ~stSkip \/ Fault = "CallbackAfterGiveUp"
          /\ cbAfterGiveUp' = (cbAfterGiveUp \/ stSkip)
          /\ answers' = answers + 1
          /\ st' = IF st = "waiting" THEN "ok" ELSE st
    /\ Close(TRUE)
    /\ UNCHANGED <<seq, susp, stopReq, panicked, reason, mbox, sent, stSkip,
                   wdVars, charged, commitPreD, commitPostD, lateDispatch,
                   hookAfterKill, stInTurn, cbStopped>>

\* The callback panics: caught, answered CallbackFailed, the actor goes on.
StatusPanic ==
    /\ ph = "turn" /\ kind = "status" /\ ~stSkip
    /\ ph' = "idle"
    /\ answers' = answers + 1
    /\ st' = IF st = "waiting" THEN "failed" ELSE st
    /\ panicked' = (Fault = "CallbackPanicStops")
    /\ cbStopped' = (Fault = "CallbackPanicStops")
    /\ Close(TRUE)
    /\ UNCHANGED <<seq, susp, stopReq, reason, mbox, sent, stSkip, wdVars,
                   charged, commitPreD, commitPostD, lateDispatch,
                   hookAfterKill, stInTurn, cbAfterGiveUp>>

\* The bug: answering the status request from inside a suspended turn.
StatusFromTurn ==
    /\ Fault = "StatusInTurn"
    /\ ph = "turn" /\ susp /\ st = "waiting"
    /\ \E i \in 1..Len(mbox) : mbox[i] = "status"
    /\ LET i == CHOOSE j \in 1..Len(mbox) : mbox[j] = "status"
       IN mbox' = SubSeq(mbox, 1, i - 1) \o SubSeq(mbox, i + 1, Len(mbox))
    /\ st' = "ok" /\ answers' = answers + 1 /\ stInTurn' = TRUE
    /\ UNCHANGED <<actorVars, sent, stSkip, wdVars, endedClean, charged,
                   commitPreD, commitPostD, lateDispatch, hookAfterKill,
                   cbAfterGiveUp, cbStopped>>

\* Exit begins between turns. stopped() opens a turn of its own, unless a
\* kill is recorded (L5); a kill during stopped() makes the reason Killed
\* and the hook finishes (L3).
BeginExit ==
    /\ ph = "idle" /\ open = FALSE /\ Exiting
    /\ IF killed /\ Fault # "KillRunsStopped"
       THEN /\ ph' = "exiting"
            /\ UNCHANGED <<kind, seq, open, hookAfterKill>>
       ELSE /\ ph' = "hook" /\ kind' = "hook" /\ seq' = seq + 1 /\ open' = TRUE
            /\ hookAfterKill' = (hookAfterKill \/ killed)
    /\ UNCHANGED <<kbit, susp, killed, stopReq, panicked, reason, mbox, sent,
                   stVars, wdVars, endedClean, charged, commitPreD, commitPostD,
                   lateDispatch, stInTurn, cbAfterGiveUp, cbStopped>>

HookEnd ==
    /\ ph = "hook"
    /\ ph' = "exiting"
    /\ Close(TRUE)
    /\ UNCHANGED <<seq, susp, stopReq, panicked, reason, mbox, sent, stVars,
                   wdVars, charged, commitPreD, commitPostD, lateDispatch,
                   hookAfterKill, stInTurn, cbAfterGiveUp, cbStopped>>

\* The death point D: the reason is fixed, queued requests are answered.
Die ==
    /\ ph = "exiting"
    /\ ph' = "dead"
    /\ reason' = FinalReason(killed)
    /\ mbox' = <<>>
    /\ AnswerAtDeath(\E i \in 1..Len(mbox) : mbox[i] = "status")
    /\ UNCHANGED <<kind, seq, open, kbit, susp, killed, stopReq, panicked,
                   sent, stSkip, wdVars, ghostVars>>

\* The runtime drops the actor at any point before D (tasks mode): cleanup
\* closes the word, consumes the kill bit, then reaches D. The reason is
\* Killed unless a non-Normal reason was requested (0.8 L4); a committed
\* kill dominates.
RuntimeDrop ==
    /\ ph \in {"start", "idle", "turn", "hook", "exiting"}
    /\ ph' = "dead"
    /\ LET k == killed \/ (kbit /\ Fault # "DropLosesKill")
       IN /\ killed' = k
          /\ reason' = IF k \/ ~(panicked \/ stopReq) THEN "Killed" ELSE FinalReason(FALSE)
    /\ open' = (open /\ Fault = "NoInvalidateAtD")
    /\ kbit' = (kbit /\ Fault = "NoInvalidateAtD")
    /\ kind' = None
    /\ mbox' = <<>>
    /\ AnswerAtDeath((\E i \in 1..Len(mbox) : mbox[i] = "status")
                     \/ (kind = "status" /\ ~stSkip))
    /\ UNCHANGED <<seq, susp, stopReq, panicked, sent, stSkip, wdVars, ghostVars>>

\* Publication: may never happen (a destructor that never returns).
Publish ==
    /\ ph = "dead"
    /\ ph' = "gone"
    /\ UNCHANGED <<kind, seq, open, kbit, susp, killed, stopReq, panicked,
                   reason, mbox, sent, stVars, wdVars, ghostVars>>

-----------------------------------------------------------------------------
\* Environment

Stop ==
    /\ ~stopReq /\ ~Past
    /\ stopReq' = TRUE
    /\ UNCHANGED <<ph, kind, seq, open, kbit, susp, killed, panicked, reason,
                   mbox, sent, stVars, wdVars, ghostVars>>

ExternalKill ==   \* ChildHandle::kill: unfenced
    /\ ~killed /\ ~Past
    /\ killed' = TRUE
    /\ UNCHANGED <<ph, kind, seq, open, kbit, susp, stopReq, panicked, reason,
                   mbox, sent, stVars, wdVars, ghostVars>>

-----------------------------------------------------------------------------
\* The watchdog (its own thread). It reads only the activity word.

Observe ==
    /\ wdPhase \in {"idle", "done"}
    /\ wdPhase' = "idle"
    /\ obs' = IF open THEN seq ELSE 0
    /\ UNCHANGED <<actorVars, mbox, sent, stVars, wdTarget, ghostVars>>

\* Correct: one CAS from (obs, open, no kill) to (obs, open, kill).
KillCas ==
    /\ Fault \notin {"CheckThenAct"}
    /\ wdPhase = "idle" /\ obs # 0
    /\ open /\ seq = obs /\ ~kbit
    /\ kbit' = TRUE
    /\ wdTarget' = obs /\ wdPhase' = "committed"
    /\ commitPreD' = (commitPreD \/ ~Past)
    /\ commitPostD' = (commitPostD \/ Past)
    /\ UNCHANGED <<ph, kind, seq, open, susp, killed, stopReq, panicked, reason,
                   mbox, sent, stVars, obs, endedClean, charged, lateDispatch,
                   hookAfterKill, stInTurn, cbAfterGiveUp, cbStopped>>

\* The CAS fails: the observed turn is no longer open. No effect.
KillCasFail ==
    /\ wdPhase = "idle" /\ obs # 0
    /\ ~(open /\ seq = obs)
    /\ obs' = 0
    /\ IF Fault = "FailedCasKills"
       THEN /\ wdPhase' = "decided" /\ wdTarget' = obs
       ELSE UNCHANGED <<wdPhase, wdTarget>>
    /\ UNCHANGED <<actorVars, mbox, sent, stVars, ghostVars>>

\* The bug: check now, kill later.
KillDecide ==
    /\ Fault = "CheckThenAct"
    /\ wdPhase = "idle" /\ obs # 0
    /\ open /\ seq = obs
    /\ wdTarget' = obs /\ wdPhase' = "decided"
    /\ UNCHANGED <<actorVars, mbox, sent, stVars, obs, ghostVars>>

\* After a successful CAS the watchdog also records the kill through the
\* ordinary path (idempotent; inert past D).
KillRecord ==
    /\ wdPhase \in {"committed", "decided"}
    /\ killed' = (killed \/ ~Past)
    /\ charged' = (charged \/ wdTarget \in endedClean)
    /\ wdPhase' = "done" /\ obs' = 0
    /\ UNCHANGED <<ph, kind, seq, open, kbit, susp, stopReq, panicked, reason,
                   mbox, sent, stVars, wdTarget, endedClean, commitPreD,
                   commitPostD, lateDispatch, hookAfterKill, stInTurn,
                   cbAfterGiveUp, cbStopped>>

-----------------------------------------------------------------------------

Next ==
    \/ StartedEnd \/ Send \/ SendStatus \/ StatusTimeout \/ Dispatch
    \/ Suspend \/ Resume \/ MsgEnd \/ MsgPanic \/ StatusEnd \/ StatusPanic
    \/ StatusFromTurn \/ BeginExit \/ HookEnd \/ Die \/ RuntimeDrop \/ Publish
    \/ Stop \/ ExternalKill
    \/ Observe \/ KillCas \/ KillCasFail \/ KillDecide \/ KillRecord

Spec == Init /\ [][Next]_vars

-----------------------------------------------------------------------------
\* Invariants

TypeOK ==
    /\ ph \in {"start", "idle", "turn", "hook", "exiting", "dead", "gone"}
    /\ kind \in {"start", "msg", "status", "hook", None}
    /\ seq \in 1..MaxSeq
    /\ open \in BOOLEAN /\ kbit \in BOOLEAN /\ susp \in BOOLEAN
    /\ killed \in BOOLEAN /\ stopReq \in BOOLEAN /\ panicked \in BOOLEAN
    /\ reason \in {None, "Normal", "Shutdown", "Panic", "Killed"}
    /\ mbox \in Seq({"msg", "status"})
    /\ st \in {"unsent", "waiting", "ok", "failed", "timeout", "stopped", "noproc"}
    /\ answers \in 0..3
    /\ wdPhase \in {"idle", "committed", "decided", "done"}
    /\ obs \in 0..MaxSeq /\ wdTarget \in 0..MaxSeq
    /\ endedClean \subseteq 1..MaxSeq

\* A watchdog kill is never charged to an activity that had already ended.
NoChargeFinishedTurn == ~charged

\* After a stop, a kill or a committed CAS, nothing is dispatched (D4, I9).
NoDispatchAfterStop == ~lateDispatch

\* A CAS committed before D makes the final reason Killed, however the
\* activity ends: return, panic, hook end or runtime drop.
KillDominates == (Past /\ commitPreD) => reason = "Killed"

\* No CAS succeeds once D has passed (the word is closed at D).
NoCommitAfterD == ~commitPostD

\* stopped() never begins after a kill (L5).
KillSkipsStopped == ~hookAfterKill

\* The status request is answered only between turns (M1).
StatusBetweenTurns == ~stInTurn

\* A status request waiting at D is answered there (R1).
StatusDischarged == Past => st # "waiting"

\* At most one answer per status request.
StatusAtMostOnce == answers <= 1

\* The callback is skipped once its caller has given up.
NoCallbackAfterGiveUp == ~cbAfterGiveUp

\* A caught callback panic does not end the actor.
CallbackPanicContained == ~cbStopped

Safety ==
    /\ TypeOK /\ NoChargeFinishedTurn /\ NoDispatchAfterStop /\ KillDominates
    /\ NoCommitAfterD /\ KillSkipsStopped /\ StatusBetweenTurns
    /\ StatusDischarged /\ StatusAtMostOnce /\ NoCallbackAfterGiveUp
    /\ CallbackPanicContained
=============================================================================
