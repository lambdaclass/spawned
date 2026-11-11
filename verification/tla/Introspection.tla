--------------------------- MODULE Introspection ---------------------------
(***************************************************************************)
(* Introspection v1 design model, revision 3 (docs/design/introspection.md, *)
(* not implemented). One actor and its activity word, an in-band status    *)
(* request (sys:get_status) and the stall watchdog's fenced kill.          *)
(*                                                                         *)
(* The activity word is (turn id, open bit, kill bit). Every activity      *)
(* opens a new turn id: started(), each message, each status request and   *)
(* stopped(). The run loop closes it with a swap that reads the kill bit;  *)
(* the watchdog commits a kill with one CAS on (observed id, open). A      *)
(* failed CAS has no effect. Every way an activity ends consumes the kill  *)
(* bit: return, panic (in started(), a handler or stopped()), the status   *)
(* turn's end, and the runtime drop's swap. D closes the word for good.    *)
(*                                                                         *)
(* Each action is one linearization point of the implementation, so a     *)
(* recorded trace event maps to exactly one action (design, "Trace events  *)
(* and their mapping"): the exit decision and the hook's begin are         *)
(* separate, the status answer and the status turn's end are separate, and *)
(* the drop swap and D are separate (an ordinary kill can come between).   *)
(*                                                                         *)
(* Ghost variables are derived from state changes in Next, never from the *)
(* Fault constant. Out-of-band reads are not actions: they change nothing. *)
(*                                                                         *)
(* Fault = "None" or a negative control that must fail (negative_checks.py *)
(* INTROSPECTION_CONTROLS, each on its named invariant):                   *)
(*   CheckThenAct, FailedCasKills, LooseCas, KillBitIgnored, PanicOverKill,*)
(*   HookPanicOverKill, StartPanicOverKill, DropLosesKill, NoInvalidateAtD,*)
(*   KillRunsStopped, LostBitRunsStopped, DispatchAfterStop, StatusInTurn, *)
(*   StatusLost, DoubleAnswer, StaleStatusReason, CallbackAfterGiveUp,     *)
(*   CallbackPanicStops, AnswerPanicStops                                  *)
(***************************************************************************)
EXTENDS Naturals, Sequences, FiniteSets

CONSTANTS
    MaxTurns,   \* messages sent
    Fault

None == "none"
MaxSeq == MaxTurns + 3   \* started(), one status turn, stopped()

VARIABLES
    ph,        \* "start", "idle", "turn", "hookwait", "hook",
               \* "exiting", "dropping", "dead" (past D), "gone" (published)
    kind,      \* open activity: "start", "msg", "status", "hook", or None
    seq,       \* activity word: turn id of the current or last activity
    open,      \* activity word: an activity is open
    kbit,      \* activity word: kill bit (set only by the watchdog's CAS)
    susp,      \* the open activity is suspended at an await (may never resume)
    killed, stopReq, panicked,
    reason,    \* final reason, fixed at D
    dropReason,\* reason fixed by the drop swap's cleanup, read at D
    mbox, sent,
    st,        \* status caller: "unsent", "waiting", "ok", "failed", "timeout", "stopped", "noproc"
    stReason,  \* the reason carried by an ActorStopped status answer
    answers,   \* answers produced for the status request
    stSkip,    \* the open status turn's caller had given up at dispatch
    stDone,    \* the open status turn has answered (or skipped)
    stFailed,  \* the open status turn's callback panicked
    stGaveUp,  \* the open status turn's caller had timed out before dispatch
    wdPhase, obs, wdTarget,
    \* ghosts, all derived in Next from state changes
    endedClean,     \* turn ids whose activity ended with no kill bit set
    charged,        \* a watchdog kill targeted an activity that had ended clean
    commitPreD,     \* a CAS succeeded before D
    commitPostD,    \* a CAS succeeded after D
    commitClosed,   \* a CAS succeeded on a word that was not open on its target
    lateDispatch,   \* a turn began after a stop, a kill or a commitment
    hookAfterKill,  \* stopped() was decided after a kill or a commitment
    stInTurn,       \* the status answer happened outside its own open turn
    cbAfterGiveUp,  \* a status callback answered after its caller timed out
    cbStopped       \* the status turn's end began exit

vars == <<ph, kind, seq, open, kbit, susp, killed, stopReq, panicked, reason,
          dropReason, mbox, sent, st, stReason, answers, stSkip, stDone,
          stFailed, stGaveUp, wdPhase, obs, wdTarget, endedClean, charged, commitPreD,
          commitPostD, commitClosed, lateDispatch, hookAfterKill, stInTurn,
          cbAfterGiveUp, cbStopped>>

\* Variables an action of the core leaves alone unless it names them.
core == <<ph, kind, seq, open, kbit, susp, killed, stopReq, panicked, reason,
          dropReason, mbox, sent, st, stReason, answers, stSkip, stDone,
          stFailed, stGaveUp, wdPhase, obs, wdTarget, endedClean>>
ghosts == <<charged, commitPreD, commitPostD, commitClosed, lateDispatch,
            hookAfterKill, stInTurn, cbAfterGiveUp, cbStopped>>

Init ==
    /\ ph = "start" /\ kind = "start" /\ seq = 1 /\ open = TRUE /\ kbit = FALSE
    /\ susp = FALSE /\ killed = FALSE /\ stopReq = FALSE /\ panicked = FALSE
    /\ reason = None /\ dropReason = None /\ mbox = <<>> /\ sent = 0
    /\ st = "unsent" /\ stReason = None /\ answers = 0
    /\ stSkip = FALSE /\ stDone = FALSE /\ stFailed = FALSE /\ stGaveUp = FALSE
    /\ wdPhase = "idle" /\ obs = 0 /\ wdTarget = 0
    /\ endedClean = {} /\ charged = FALSE /\ commitPreD = FALSE
    /\ commitPostD = FALSE /\ commitClosed = FALSE /\ lateDispatch = FALSE
    /\ hookAfterKill = FALSE /\ stInTurn = FALSE /\ cbAfterGiveUp = FALSE
    /\ cbStopped = FALSE

Exiting == killed \/ stopReq \/ panicked
Past == ph \in {"dead", "gone"}
Committed == commitPreD

\* Closing the activity word: the swap reads the kill bit.
Close(consume) ==
    /\ open' = FALSE /\ kbit' = FALSE /\ kind' = None /\ susp' = FALSE
    /\ killed' = (killed \/ (kbit /\ consume))
    /\ endedClean' = IF kbit THEN endedClean ELSE endedClean \cup {seq}

FinalReason(k, p) ==
    IF k THEN "Killed" ELSE IF p THEN "Panic"
    ELSE IF stopReq THEN "Shutdown" ELSE "Normal"

-----------------------------------------------------------------------------
\* The actor

StartedEnd ==
    /\ ph = "start" /\ ~susp
    /\ ph' = "idle"
    /\ Close(TRUE)
    /\ UNCHANGED <<seq, stopReq, panicked, reason, dropReason, mbox, sent, st,
                   stReason, answers, stSkip, stDone, stFailed, stGaveUp, wdPhase, obs, wdTarget>>

\* A panic in started(): no stopped() (L5); a committed kill still wins.
StartPanic ==
    /\ ph = "start" /\ ~susp
    /\ ph' = "exiting"
    /\ panicked' = TRUE
    /\ Close(Fault # "StartPanicOverKill")
    /\ UNCHANGED <<seq, stopReq, reason, dropReason, mbox, sent, st, stReason,
                   answers, stSkip, stDone, stFailed, stGaveUp, wdPhase, obs, wdTarget>>

Send ==
    /\ sent < MaxTurns
    /\ sent' = sent + 1
    /\ mbox' = IF Past THEN mbox ELSE Append(mbox, "msg")
    /\ UNCHANGED <<ph, kind, seq, open, kbit, susp, killed, stopReq, panicked,
                   reason, dropReason, st, stReason, answers, stSkip, stDone,
                   stFailed, stGaveUp, wdPhase, obs, wdTarget, endedClean>>

SendStatus ==
    /\ st = "unsent"
    /\ IF Past THEN /\ st' = "noproc" /\ mbox' = mbox
               ELSE /\ st' = "waiting" /\ mbox' = Append(mbox, "status")
    /\ UNCHANGED <<ph, kind, seq, open, kbit, susp, killed, stopReq, panicked,
                   reason, dropReason, sent, stReason, answers, stSkip, stDone,
                   stFailed, stGaveUp, wdPhase, obs, wdTarget, endedClean>>

\* The caller's deadline passes (the slot closes; a later answer is dropped).
StatusTimeout ==
    /\ st = "waiting"
    /\ st' = "timeout"
    /\ UNCHANGED <<ph, kind, seq, open, kbit, susp, killed, stopReq, panicked,
                   reason, dropReason, mbox, sent, stReason, answers, stSkip,
                   stDone, stFailed, stGaveUp, wdPhase, obs, wdTarget, endedClean>>

\* Dispatch checks for exit before every receive (D4). Every turn, status
\* included, opens a new turn id. The status callback's admission is
\* decided here against the slot: a closed slot means skip.
Dispatch ==
    /\ ph = "idle" /\ ~open
    /\ ~Exiting \/ Fault = "DispatchAfterStop"
    /\ mbox # <<>>
    /\ mbox' = Tail(mbox)
    /\ ph' = "turn" /\ seq' = seq + 1 /\ open' = TRUE /\ susp' = FALSE
    /\ kind' = Head(mbox)
    /\ stSkip' = (Head(mbox) = "status" /\ st = "timeout"
                  /\ Fault # "CallbackAfterGiveUp")
    /\ stDone' = (Head(mbox) = "status" /\ st = "timeout"
                  /\ Fault # "CallbackAfterGiveUp")
    /\ stFailed' = FALSE
    /\ stGaveUp' = (Head(mbox) = "status" /\ st = "timeout")
    /\ UNCHANGED <<kbit, killed, stopReq, panicked, reason, dropReason, sent,
                   st, stReason, answers, wdPhase, obs, wdTarget, endedClean>>

\* Activities may suspend at an await and never resume (no fairness); a
\* status callback is synchronous and never suspends.
Suspend ==
    /\ ph \in {"start", "turn", "hook"} /\ kind \in {"start", "msg", "hook"} /\ ~susp
    /\ susp' = TRUE
    /\ UNCHANGED <<ph, kind, seq, open, kbit, killed, stopReq, panicked, reason,
                   dropReason, mbox, sent, st, stReason, answers, stSkip, stDone,
                   stFailed, stGaveUp, wdPhase, obs, wdTarget, endedClean>>

Resume ==
    /\ susp
    /\ susp' = FALSE
    /\ UNCHANGED <<ph, kind, seq, open, kbit, killed, stopReq, panicked, reason,
                   dropReason, mbox, sent, st, stReason, answers, stSkip, stDone,
                   stFailed, stGaveUp, wdPhase, obs, wdTarget, endedClean>>

MsgEnd ==
    /\ ph = "turn" /\ kind = "msg" /\ ~susp
    /\ ph' = "idle"
    /\ Close(Fault # "KillBitIgnored" /\ Fault # "LostBitRunsStopped")
    /\ UNCHANGED <<seq, stopReq, panicked, reason, dropReason, mbox, sent, st,
                   stReason, answers, stSkip, stDone, stFailed, stGaveUp, wdPhase, obs, wdTarget>>

\* A handler panics: the turn ends and exit begins; a committed kill wins.
MsgPanic ==
    /\ ph = "turn" /\ kind = "msg" /\ ~susp
    /\ ph' = "idle"
    /\ panicked' = TRUE
    /\ Close(Fault # "PanicOverKill")
    /\ UNCHANGED <<seq, stopReq, reason, dropReason, mbox, sent, st, stReason,
                   answers, stSkip, stDone, stFailed, stGaveUp, wdPhase, obs, wdTarget>>

\* The status answer, at the reply slot's discharge (its own linearization
\* point, before the end swap). The callback either returns or panics.
StatusAnswer ==
    /\ ph = "turn" /\ kind = "status" /\ ~stDone
    /\ \E fail \in BOOLEAN :
        /\ stFailed' = fail
        /\ st' = IF st = "waiting" THEN (IF fail THEN "failed" ELSE "ok") ELSE st
        \* AnswerPanicStops: the caught panic is turned into a handler panic
        \* while the callback runs, before the turn ends.
        /\ panicked' = (panicked \/ (fail /\ Fault = "AnswerPanicStops"))
    /\ answers' = answers + 1
    /\ stDone' = TRUE
    /\ UNCHANGED <<ph, kind, seq, open, kbit, susp, killed, stopReq,
                   reason, dropReason, mbox, sent, stReason, stSkip, stGaveUp, wdPhase,
                   obs, wdTarget, endedClean>>

\* The bug: answering the status request from inside a suspended message turn.
StatusFromTurn ==
    /\ Fault = "StatusInTurn"
    /\ ph = "turn" /\ kind = "msg" /\ susp /\ st = "waiting"
    /\ \E i \in 1..Len(mbox) : mbox[i] = "status"
    /\ LET i == CHOOSE j \in 1..Len(mbox) : mbox[j] = "status"
       IN mbox' = SubSeq(mbox, 1, i - 1) \o SubSeq(mbox, i + 1, Len(mbox))
    /\ st' = "ok" /\ answers' = answers + 1
    /\ UNCHANGED <<ph, kind, seq, open, kbit, susp, killed, stopReq, panicked,
                   reason, dropReason, sent, stReason, stSkip, stDone, stFailed, stGaveUp,
                   wdPhase, obs, wdTarget, endedClean>>

\* The status turn ends (the end swap). A caught callback panic is not a
\* handler panic.
StatusEnd ==
    /\ ph = "turn" /\ kind = "status" /\ stDone
    /\ ph' = "idle"
    /\ panicked' = (panicked \/ (stFailed /\ Fault = "CallbackPanicStops"))
    /\ Close(TRUE)
    /\ UNCHANGED <<seq, stopReq, reason, dropReason, mbox, sent, st, stReason,
                   answers, stSkip, stDone, stFailed, stGaveUp, wdPhase, obs, wdTarget>>

\* Exit begins between turns: the exit request decides whether stopped()
\* runs (0.8 begin_hook, under its lock); it is skipped after a kill (L5).
ExitDecide ==
    /\ ph = "idle" /\ ~open /\ Exiting
    /\ ph' = IF killed /\ Fault # "KillRunsStopped" THEN "exiting" ELSE "hookwait"
    /\ UNCHANGED <<kind, seq, open, kbit, susp, killed, stopReq, panicked,
                   reason, dropReason, mbox, sent, st, stReason, answers,
                   stSkip, stDone, stFailed, stGaveUp, wdPhase, obs, wdTarget, endedClean>>

\* stopped() begins: its activity opens (word.store(open)).
HookBegin ==
    /\ ph = "hookwait"
    /\ ph' = "hook" /\ kind' = "hook" /\ seq' = seq + 1 /\ open' = TRUE
    /\ UNCHANGED <<kbit, susp, killed, stopReq, panicked, reason, dropReason,
                   mbox, sent, st, stReason, answers, stSkip, stDone, stFailed, stGaveUp,
                   wdPhase, obs, wdTarget, endedClean>>

\* stopped() returns; a kill during it makes the reason Killed (L3).
HookEnd ==
    /\ ph = "hook" /\ ~susp
    /\ ph' = "exiting"
    /\ Close(TRUE)
    /\ UNCHANGED <<seq, stopReq, panicked, reason, dropReason, mbox, sent, st,
                   stReason, answers, stSkip, stDone, stFailed, stGaveUp, wdPhase, obs, wdTarget>>

\* stopped() panics: Panic unless a kill was received.
HookPanic ==
    /\ ph = "hook" /\ ~susp
    /\ ph' = "exiting"
    /\ panicked' = TRUE
    /\ Close(Fault # "HookPanicOverKill")
    /\ UNCHANGED <<seq, stopReq, reason, dropReason, mbox, sent, st, stReason,
                   answers, stSkip, stDone, stFailed, stGaveUp, wdPhase, obs, wdTarget>>

\* The runtime drops the actor before D (tasks mode): cleanup swaps the word
\* to DEAD, consuming the kill bit. The reason is decided at D.
DropSwap ==
    /\ ph \in {"start", "idle", "turn", "hookwait", "hook", "exiting"}
    /\ ph' = "dropping"
    /\ killed' = (killed \/ (kbit /\ Fault # "DropLosesKill"))
    /\ open' = (open /\ Fault = "NoInvalidateAtD")
    /\ kbit' = (kbit /\ Fault = "NoInvalidateAtD")
    /\ kind' = IF Fault = "NoInvalidateAtD" THEN kind ELSE None
    /\ susp' = FALSE
    /\ endedClean' = IF kbit \/ ~open THEN endedClean ELSE endedClean \cup {seq}
    /\ UNCHANGED <<seq, stopReq, panicked, reason, dropReason, mbox, sent, st,
                   stReason, answers, stSkip, stDone, stFailed, stGaveUp, wdPhase, obs, wdTarget>>

\* The death point D. After a drop the reason is Killed unless a non-Normal
\* reason was requested (0.8 L4); a recorded kill dominates. Every waiting
\* status request is answered with the final reason.
Die ==
    /\ ph \in {"exiting", "dropping"}
    /\ ph' = "dead"
    /\ LET r == IF ph = "dropping" /\ ~killed /\ ~panicked /\ ~stopReq
                THEN "Killed" ELSE FinalReason(killed, panicked)
       IN /\ reason' = r
          /\ stReason' = IF st = "waiting" /\ Fault # "StatusLost"
                         THEN (IF Fault = "StaleStatusReason"
                               THEN FinalReason(FALSE, panicked) ELSE r)
                         ELSE stReason
    /\ st' = IF st = "waiting" /\ Fault # "StatusLost" THEN "stopped" ELSE st
    /\ answers' = answers + (IF (st \in {"waiting", "timeout"} /\ ~stDone
                                 /\ ("status" \in {mbox[i] : i \in 1..Len(mbox)}
                                     \/ kind = "status"))
                                \/ Fault = "DoubleAnswer" THEN 1 ELSE 0)
    /\ mbox' = <<>>
    /\ open' = (open /\ Fault = "NoInvalidateAtD")
    /\ UNCHANGED <<kind, seq, kbit, susp, killed, stopReq, panicked,
                   dropReason, sent, stSkip, stDone, stFailed, stGaveUp, wdPhase, obs,
                   wdTarget, endedClean>>

Publish ==
    /\ ph = "dead"
    /\ ph' = "gone"
    /\ UNCHANGED <<kind, seq, open, kbit, susp, killed, stopReq, panicked,
                   reason, dropReason, mbox, sent, st, stReason, answers,
                   stSkip, stDone, stFailed, stGaveUp, wdPhase, obs, wdTarget, endedClean>>

-----------------------------------------------------------------------------
\* Environment

Stop ==
    /\ ~stopReq /\ ~Past
    /\ stopReq' = TRUE
    /\ UNCHANGED <<ph, kind, seq, open, kbit, susp, killed, panicked, reason,
                   dropReason, mbox, sent, st, stReason, answers, stSkip,
                   stDone, stFailed, stGaveUp, wdPhase, obs, wdTarget, endedClean>>

ExternalKill ==   \* ChildHandle::kill: unfenced; possible until D
    /\ ~killed /\ ~Past
    /\ killed' = TRUE
    /\ UNCHANGED <<ph, kind, seq, open, kbit, susp, stopReq, panicked, reason,
                   dropReason, mbox, sent, st, stReason, answers, stSkip,
                   stDone, stFailed, stGaveUp, wdPhase, obs, wdTarget, endedClean>>

-----------------------------------------------------------------------------
\* The watchdog (its own thread). It reads only the activity word.

Observe ==
    /\ wdPhase \in {"idle", "done"}
    /\ wdPhase' = "idle"
    /\ obs' = IF open THEN seq ELSE 0
    /\ UNCHANGED <<ph, kind, seq, open, kbit, susp, killed, stopReq, panicked,
                   reason, dropReason, mbox, sent, st, stReason, answers,
                   stSkip, stDone, stFailed, stGaveUp, wdTarget, endedClean>>

\* One CAS from (obs, open, no kill) to (obs, open, kill). LooseCas compares
\* the turn id only.
KillCas ==
    /\ Fault # "CheckThenAct"
    /\ wdPhase = "idle" /\ obs # 0
    /\ seq = obs /\ ~kbit /\ (open \/ Fault = "LooseCas")
    /\ kbit' = TRUE
    /\ wdTarget' = obs /\ wdPhase' = "committed"
    /\ UNCHANGED <<ph, kind, seq, open, susp, killed, stopReq, panicked, reason,
                   dropReason, mbox, sent, st, stReason, answers, stSkip,
                   stDone, stFailed, stGaveUp, obs, endedClean>>

\* The CAS fails: no effect (FailedCasKills: it records a kill anyway).
KillCasFail ==
    /\ wdPhase = "idle" /\ obs # 0
    /\ ~(seq = obs /\ ~kbit /\ (open \/ Fault = "LooseCas"))
    /\ obs' = 0
    /\ IF Fault = "FailedCasKills"
       THEN /\ wdPhase' = "decided" /\ wdTarget' = obs
       ELSE UNCHANGED <<wdPhase, wdTarget>>
    /\ UNCHANGED <<ph, kind, seq, open, kbit, susp, killed, stopReq, panicked,
                   reason, dropReason, mbox, sent, st, stReason, answers,
                   stSkip, stDone, stFailed, stGaveUp, endedClean>>

\* The bug: check now, kill later.
KillDecide ==
    /\ Fault = "CheckThenAct"
    /\ wdPhase = "idle" /\ obs # 0
    /\ open /\ seq = obs
    /\ wdTarget' = obs /\ wdPhase' = "decided"
    /\ UNCHANGED <<ph, kind, seq, open, kbit, susp, killed, stopReq, panicked,
                   reason, dropReason, mbox, sent, st, stReason, answers,
                   stSkip, stDone, stFailed, stGaveUp, obs, endedClean>>

\* After its CAS the watchdog records the kill through the ordinary path
\* (idempotent; inert past D).
KillRecord ==
    /\ wdPhase \in {"committed", "decided"}
    /\ killed' = (killed \/ ~Past)
    /\ wdPhase' = "done" /\ obs' = 0
    /\ UNCHANGED <<ph, kind, seq, open, kbit, susp, stopReq, panicked, reason,
                   dropReason, mbox, sent, st, stReason, answers, stSkip,
                   stDone, stFailed, stGaveUp, wdTarget, endedClean>>

-----------------------------------------------------------------------------

Core ==
    \/ StartedEnd \/ StartPanic \/ Send \/ SendStatus \/ StatusTimeout
    \/ Dispatch \/ Suspend \/ Resume \/ MsgEnd \/ MsgPanic
    \/ StatusAnswer \/ StatusFromTurn \/ StatusEnd
    \/ ExitDecide \/ HookBegin \/ HookEnd \/ HookPanic
    \/ DropSwap \/ Die \/ Publish \/ Stop \/ ExternalKill
    \/ Observe \/ KillCas \/ KillCasFail \/ KillDecide \/ KillRecord

\* Ghosts, from the state change of each step.
Ghosts ==
    /\ charged' = (charged \/ (wdPhase' \in {"committed", "decided", "done"}
                              /\ wdTarget' \in endedClean'))
    /\ commitPreD' = (commitPreD \/ (kbit' /\ ~kbit /\ ~Past))
    /\ commitPostD' = (commitPostD \/ (kbit' /\ ~kbit /\ Past))
    /\ commitClosed' = (commitClosed \/ (kbit' /\ ~kbit /\ ~open))
    /\ lateDispatch' = (lateDispatch \/ (open' /\ seq' # seq /\ kind' \in {"msg", "status"}
                                         /\ (Exiting \/ Committed)))
    /\ hookAfterKill' = (hookAfterKill \/ (ph = "idle" /\ ph' = "hookwait"
                                           /\ (killed \/ Committed)))
    /\ stInTurn' = (stInTurn \/ (st = "waiting" /\ st' \in {"ok", "failed"}
                                 /\ ~(ph = "turn" /\ kind = "status")))
    /\ cbAfterGiveUp' = (cbAfterGiveUp \/ (stGaveUp /\ answers' > answers
                                           /\ ph = "turn" /\ kind = "status"))
    \* Over the whole status turn (answer and end): no step taken while it is
    \* open records a panic.
    /\ cbStopped' = (cbStopped \/ (ph = "turn" /\ kind = "status"
                                   /\ ~panicked /\ panicked'))

Next == Core /\ Ghosts

Spec == Init /\ [][Next]_vars

-----------------------------------------------------------------------------
\* Invariants

TypeOK ==
    /\ ph \in {"start", "idle", "turn", "hookwait", "hook",
               "exiting", "dropping", "dead", "gone"}
    /\ kind \in {"start", "msg", "status", "hook", None}
    /\ seq \in 1..MaxSeq
    /\ open \in BOOLEAN /\ kbit \in BOOLEAN /\ susp \in BOOLEAN
    /\ killed \in BOOLEAN /\ stopReq \in BOOLEAN /\ panicked \in BOOLEAN
    /\ reason \in {None, "Normal", "Shutdown", "Panic", "Killed"}
    /\ stReason \in {None, "Normal", "Shutdown", "Panic", "Killed"}
    /\ mbox \in Seq({"msg", "status"})
    /\ st \in {"unsent", "waiting", "ok", "failed", "timeout", "stopped", "noproc"}
    /\ answers \in 0..3
    /\ wdPhase \in {"idle", "committed", "decided", "done"}
    /\ obs \in 0..MaxSeq /\ wdTarget \in 0..MaxSeq
    /\ endedClean \subseteq 1..MaxSeq

\* A watchdog kill never targets an activity that had already ended, checked
\* from the moment of the commitment (or decision) on.
NoChargeFinishedTurn == ~charged

\* A commitment is only ever made on the open target activity.
CommitOnOpen == ~commitClosed

\* After a stop, a kill or a commitment, no message or status turn begins.
NoDispatchAfterStop == ~lateDispatch

\* A commitment before D makes the final reason Killed, however the
\* activity ends (return, panic in started(), a handler or stopped(), drop).
KillDominates == (Past /\ commitPreD) => reason = "Killed"

NoCommitAfterD == ~commitPostD

\* stopped() is never decided after a kill or a commitment (L5).
KillSkipsStopped == ~hookAfterKill

\* The status answer happens only inside its own turn (M1).
StatusBetweenTurns == ~stInTurn

\* A waiting status request is answered at D, with the final reason (R1).
StatusDischarged == Past => st # "waiting"
StatusReasonIsFinal == (Past /\ st = "stopped") => stReason = reason

StatusAtMostOnce == answers <= 1

\* No callback answers after its caller gave up before dispatch.
NoCallbackAfterGiveUp == ~cbAfterGiveUp

\* No step of a status turn begins exit.
CallbackPanicContained == ~cbStopped

Safety ==
    /\ TypeOK /\ NoChargeFinishedTurn /\ CommitOnOpen /\ NoDispatchAfterStop
    /\ KillDominates /\ NoCommitAfterD /\ KillSkipsStopped
    /\ StatusBetweenTurns /\ StatusDischarged /\ StatusReasonIsFinal
    /\ StatusAtMostOnce /\ NoCallbackAfterGiveUp /\ CallbackPanicContained
=============================================================================
