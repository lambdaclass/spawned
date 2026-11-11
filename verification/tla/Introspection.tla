--------------------------- MODULE Introspection ---------------------------
(***************************************************************************)
(* Introspection v1 design model (docs/design/introspection.md, not       *)
(* implemented). One actor, its mailbox, an in-band status request         *)
(* (sys:get_status) and the stall watchdog's fenced kill.                  *)
(*                                                                         *)
(* What it checks:                                                         *)
(*  - the watchdog kills only the turn it observed: the kill is a CAS on   *)
(*    the actor's activity word (turn id + in-turn bit + kill bit); a turn *)
(*    that ended before the CAS is never charged, and a committed kill     *)
(*    lets no later turn dispatch (FencedKill);                            *)
(*  - a status request is answered only between turns, never while a      *)
(*    turn is suspended at an await (StatusBetweenTurns, M1);              *)
(*  - a status request has exactly one outcome, and one still queued at    *)
(*    the death point is answered there (StatusDischarged, R1);            *)
(*  - a kill never runs stopped(), and nothing dispatches after a stop is  *)
(*    observed (KillSkipsStopped, NoDispatchAfterStop).                    *)
(*                                                                         *)
(* Out-of-band reads (process_info, snapshots, the watchdog's sampling)   *)
(* are not actions: they change no variable, which is the point (I4).      *)
(*                                                                         *)
(* Fault = "None" or a negative control that must fail:                    *)
(*   CheckThenAct    watchdog checks the turn, kills in a later step       *)
(*   KillBitIgnored  turn end drops the kill bit set by the CAS            *)
(*   StatusInTurn    status answered while a turn is suspended            *)
(*   StatusLost      a queued status request is not answered at death     *)
(*   KillRunsStopped stopped() runs after a kill                           *)
(*   DispatchAfterStop the run loop dispatches after a stop is requested   *)
(***************************************************************************)
EXTENDS Naturals, Sequences

CONSTANTS
    MaxTurns,   \* messages sent
    Fault

None == "none"

VARIABLES
    ph,        \* "start", "idle", "turn", "exiting", "dead"
    seq,       \* id of the current or last turn (the activity word's counter)
    susp,      \* the turn is suspended at an await (not polling)
    kbit,      \* kill bit in the activity word (set by the watchdog's CAS)
    killed,    \* a kill is recorded (ExitRequest)
    stopReq,   \* a cooperative stop is recorded
    mbox,      \* queued items: "msg" or "status"
    sent,      \* messages sent so far
    st,        \* status request: "unsent", "queued", "ok", "stopped", "noproc"
    gaveUp,    \* the status caller timed out (the request stays queued, R4)
    stInTurn,  \* ghost: status answered while a turn was open
    obs,       \* watchdog: turn id observed in a turn, or 0
    wdTarget,  \* watchdog: turn id its kill targets, or 0
    wdPhase,   \* watchdog: "idle", "decided" (CheckThenAct), "cas", "done"
    stoppedRan, \* ghost: stopped() ran
    hookKilled, \* ghost: stopped() ran although a kill was recorded first
    lateTurn    \* ghost: a turn was dispatched after a stop was recorded

vars == <<ph, seq, susp, kbit, killed, stopReq, mbox, sent, st, gaveUp,
          stInTurn, obs, wdTarget, wdPhase, stoppedRan, hookKilled, lateTurn>>

Init ==
    /\ ph = "start" /\ seq = 0 /\ susp = FALSE /\ kbit = FALSE
    /\ killed = FALSE /\ stopReq = FALSE /\ mbox = <<>> /\ sent = 0
    /\ st = "unsent" /\ gaveUp = FALSE /\ stInTurn = FALSE
    /\ obs = 0 /\ wdTarget = 0 /\ wdPhase = "idle" /\ stoppedRan = FALSE
    /\ hookKilled = FALSE /\ lateTurn = FALSE

Stopping == killed \/ stopReq
Dead == ph = "dead"

-----------------------------------------------------------------------------
\* The actor

Started ==
    /\ ph = "start"
    /\ ph' = "idle"
    /\ UNCHANGED <<seq, susp, kbit, killed, stopReq, mbox, sent, st, gaveUp,
                   stInTurn, obs, wdTarget, wdPhase, stoppedRan, hookKilled, lateTurn>>

\* A client sends a message; past the death point it is refused (NoProc).
Send ==
    /\ sent < MaxTurns
    /\ sent' = sent + 1
    /\ mbox' = IF Dead THEN mbox ELSE Append(mbox, "msg")
    /\ UNCHANGED <<ph, seq, susp, kbit, killed, stopReq, st, gaveUp,
                   stInTurn, obs, wdTarget, wdPhase, stoppedRan, hookKilled, lateTurn>>

\* get_status: an ordinary request (the in-band system message).
SendStatus ==
    /\ st = "unsent"
    /\ IF Dead THEN /\ st' = "noproc" /\ mbox' = mbox
               ELSE /\ st' = "queued" /\ mbox' = Append(mbox, "status")
    /\ UNCHANGED <<ph, seq, susp, kbit, killed, stopReq, sent, gaveUp,
                   stInTurn, obs, wdTarget, wdPhase, stoppedRan, hookKilled, lateTurn>>

StatusTimeout ==
    /\ st = "queued" /\ ~gaveUp
    /\ gaveUp' = TRUE
    /\ UNCHANGED <<ph, seq, susp, kbit, killed, stopReq, mbox, sent, st,
                   stInTurn, obs, wdTarget, wdPhase, stoppedRan, hookKilled, lateTurn>>

\* The run loop checks for a stop before every receive (D4).
Dispatch ==
    /\ ph = "idle"
    /\ ~Stopping \/ Fault = "DispatchAfterStop"
    /\ mbox # <<>>
    /\ mbox' = Tail(mbox)
    /\ IF Head(mbox) = "msg"
       THEN /\ ph' = "turn" /\ seq' = seq + 1 /\ susp' = FALSE
            /\ lateTurn' = (lateTurn \/ Stopping)
            /\ UNCHANGED <<st, stInTurn>>
       ELSE \* the status turn: format_status runs and answers at once
            /\ st' = "ok"
            /\ stInTurn' = stInTurn
            /\ UNCHANGED <<ph, seq, susp, lateTurn>>
    /\ UNCHANGED <<kbit, killed, stopReq, sent, gaveUp, obs, wdTarget,
                   wdPhase, stoppedRan, hookKilled>>

\* The bug: answering the status request from inside an open turn.
StatusFromTurn ==
    /\ Fault = "StatusInTurn"
    /\ ph = "turn" /\ susp
    /\ \E i \in 1..Len(mbox) : mbox[i] = "status"
    /\ LET i == CHOOSE j \in 1..Len(mbox) : mbox[j] = "status"
       IN mbox' = SubSeq(mbox, 1, i - 1) \o SubSeq(mbox, i + 1, Len(mbox))
    /\ st' = "ok" /\ stInTurn' = TRUE
    /\ UNCHANGED <<ph, seq, susp, kbit, killed, stopReq, sent, gaveUp,
                   obs, wdTarget, wdPhase, stoppedRan, hookKilled, lateTurn>>

\* A turn suspends at an await and may never resume (no fairness): the
\* wedged `.await` of the ethrex connection actor.
Suspend ==
    /\ ph = "turn" /\ ~susp
    /\ susp' = TRUE
    /\ UNCHANGED <<ph, seq, kbit, killed, stopReq, mbox, sent, st, gaveUp,
                   stInTurn, obs, wdTarget, wdPhase, stoppedRan, hookKilled, lateTurn>>

Resume ==
    /\ ph = "turn" /\ susp
    /\ susp' = FALSE
    /\ UNCHANGED <<ph, seq, kbit, killed, stopReq, mbox, sent, st, gaveUp,
                   stInTurn, obs, wdTarget, wdPhase, stoppedRan, hookKilled, lateTurn>>

\* Turn end swaps the activity word to idle; a kill bit in the old value is
\* recorded as a kill before the next receive.
TurnEnd ==
    /\ ph = "turn" /\ ~susp
    /\ ph' = "idle"
    /\ kbit' = FALSE
    /\ killed' = (killed \/ (kbit /\ Fault # "KillBitIgnored"))
    /\ UNCHANGED <<seq, susp, stopReq, mbox, sent, st, gaveUp, stInTurn,
                   obs, wdTarget, wdPhase, stoppedRan, hookKilled, lateTurn>>

\* Exit begins between turns. stopped() runs after a cooperative stop, not
\* after a kill (L5).
BeginExit ==
    /\ ph = "idle" /\ Stopping
    /\ ph' = "exiting"
    /\ stoppedRan' = (~killed \/ Fault = "KillRunsStopped")
    /\ hookKilled' = (killed /\ stoppedRan')
    /\ UNCHANGED <<seq, susp, kbit, killed, stopReq, mbox, sent, st, gaveUp,
                   stInTurn, obs, wdTarget, wdPhase, lateTurn>>

\* The death point: queued requests are answered with the final reason.
Die ==
    /\ ph = "exiting"
    /\ ph' = "dead"
    /\ mbox' = <<>>
    /\ st' = IF st = "queued" /\ Fault # "StatusLost" THEN "stopped" ELSE st
    /\ UNCHANGED <<seq, susp, kbit, killed, stopReq, sent, gaveUp, stInTurn,
                   obs, wdTarget, wdPhase, stoppedRan, hookKilled, lateTurn>>

-----------------------------------------------------------------------------
\* Environment: administrative stop and an unfenced kill (ChildHandle::kill)

Stop ==
    /\ ~stopReq /\ ~Dead
    /\ stopReq' = TRUE
    /\ UNCHANGED <<ph, seq, susp, kbit, killed, mbox, sent, st, gaveUp,
                   stInTurn, obs, wdTarget, wdPhase, stoppedRan, hookKilled, lateTurn>>

\* ChildHandle::kill: unfenced, recorded at once (it ends the actor when
\* the open turn returns: L3, nothing preempts a turn).
ExternalKill ==
    /\ ~killed /\ ~Dead
    /\ killed' = TRUE
    /\ UNCHANGED <<ph, seq, susp, kbit, stopReq, mbox, sent, st, gaveUp,
                   stInTurn, obs, wdTarget, wdPhase, stoppedRan, hookKilled, lateTurn>>

-----------------------------------------------------------------------------
\* The watchdog (its own OS thread). Sampling reads the activity word only.

Observe ==
    /\ wdPhase = "idle"
    /\ obs' = IF ph = "turn" THEN seq ELSE 0
    /\ UNCHANGED <<ph, seq, susp, kbit, killed, stopReq, mbox, sent, st,
                   gaveUp, stInTurn, wdTarget, wdPhase, stoppedRan, hookKilled, lateTurn>>

\* Correct: one CAS from (obs, in turn) to (obs, in turn, kill bit).
KillCas ==
    /\ Fault # "CheckThenAct"
    /\ wdPhase = "idle" /\ obs # 0
    /\ ph = "turn" /\ seq = obs
    /\ kbit' = TRUE
    /\ wdTarget' = obs
    /\ wdPhase' = "cas"
    /\ UNCHANGED <<ph, seq, susp, killed, stopReq, mbox, sent, st, gaveUp,
                   stInTurn, obs, stoppedRan, hookKilled, lateTurn>>

\* The bug: check now, kill later.
KillDecide ==
    /\ Fault = "CheckThenAct"
    /\ wdPhase = "idle" /\ obs # 0
    /\ ph = "turn" /\ seq = obs
    /\ wdTarget' = obs
    /\ wdPhase' = "decided"
    /\ UNCHANGED <<ph, seq, susp, kbit, killed, stopReq, mbox, sent, st,
                   gaveUp, stInTurn, obs, stoppedRan, hookKilled, lateTurn>>

\* After its CAS (or decision) the watchdog records the kill through the
\* ordinary kill path. Recording twice is idempotent.
KillRecord ==
    /\ wdPhase \in {"cas", "decided"}
    /\ killed' = (killed \/ ~Dead)
    /\ wdPhase' = "done"
    /\ UNCHANGED <<ph, seq, susp, kbit, stopReq, mbox, sent, st, gaveUp,
                   stInTurn, obs, wdTarget, stoppedRan, hookKilled, lateTurn>>

-----------------------------------------------------------------------------

Next ==
    \/ Started \/ Send \/ SendStatus \/ StatusTimeout \/ Dispatch
    \/ StatusFromTurn \/ Suspend \/ Resume \/ TurnEnd \/ BeginExit \/ Die
    \/ Stop \/ ExternalKill \/ Observe \/ KillCas \/ KillDecide \/ KillRecord

Spec == Init /\ [][Next]_vars

-----------------------------------------------------------------------------
\* Invariants

TypeOK ==
    /\ ph \in {"start", "idle", "turn", "exiting", "dead"}
    /\ seq \in 0..MaxTurns
    /\ susp \in BOOLEAN /\ kbit \in BOOLEAN /\ killed \in BOOLEAN
    /\ stopReq \in BOOLEAN /\ gaveUp \in BOOLEAN /\ stInTurn \in BOOLEAN
    /\ stoppedRan \in BOOLEAN /\ hookKilled \in BOOLEAN /\ lateTurn \in BOOLEAN
    /\ mbox \in Seq({"msg", "status"})
    /\ st \in {"unsent", "queued", "ok", "stopped", "noproc"}
    /\ obs \in 0..MaxTurns /\ wdTarget \in 0..MaxTurns
    /\ wdPhase \in {"idle", "decided", "cas", "done"}

\* A watchdog kill hits the turn it observed: once it is committed, no
\* later turn has been or will be dispatched.
FencedKill == wdPhase = "done" => seq = wdTarget

\* The kill bit is only ever set on the observed, still-open turn.
KillBitOnTarget == kbit => (ph = "turn" /\ seq = wdTarget)

StatusBetweenTurns == ~stInTurn

StatusDischarged == Dead => st # "queued"

KillSkipsStopped == ~hookKilled

\* After a stop or kill is recorded, no new turn starts (D4).
NoDispatchAfterStop == ~lateTurn

Safety ==
    /\ TypeOK /\ FencedKill /\ KillBitOnTarget /\ StatusBetweenTurns
    /\ StatusDischarged /\ KillSkipsStopped /\ NoDispatchAfterStop
=============================================================================
