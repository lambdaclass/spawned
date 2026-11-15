-------------------------- MODULE Coordinator --------------------------
EXTENDS Naturals, FiniteSets
(* The ActorSystem shutdown coordinator (begin_shutdown in                  *)
(* concurrency/src/system.rs). Targets are the admitted roots in STOP      *)
(* order: target 1 is the last-admitted root (the Rust loop walks          *)
(* `targets.iter().rev()`), so "lower index stops first" is "later         *)
(* admitted stops first". Targets abstract roots whose startup committed;  *)
(* an unready supervisor startup rolls itself back and is not modelled.    *)
(*                                                                         *)
(* v0.8 (E3): the ordered loop sends each root its parent's Shutdown exit  *)
(* signal; when the grace expires, the remaining roots get the kill        *)
(* (`killed`), so a trapping root stuck in stopped() cannot hold the join. *)
(* The panic fallback (emergency_shutdown) still stops with Shutdown. A    *)
(* supervisor root applies its own per-child kill (Supervision.tla).       *)
(* `@type:` comments are Apalache annotations (CoordinatorInd.tla); TLC    *)
(* ignores them.                                                           *)
CONSTANTS
    \* @type: Int;
    TargetCount,
    \* @type: Str;
    Fault
Targets == 1..TargetCount
VARIABLES
    \* @type: Str;
    phase,
    \* @type: Bool;
    open,
    \* @type: Set(Int);
    signalled,
    \* @type: Set(Int);
    finished,
    \* @type: Int;
    index,
    \* @type: Bool;
    crashed,
    \* @type: Bool;
    reported,
    \* @type: Bool;
    released,
    \* @type: Bool;
    expired,
    \* @type: Set(Int);
    killed
vars == <<phase, open, signalled, finished, index, crashed, reported, released, expired, killed>>

Init == /\ phase = "Running" /\ open = TRUE
        /\ signalled = {} /\ finished = {} /\ index = 1
        /\ crashed = FALSE /\ reported = FALSE /\ released = FALSE
        /\ expired = FALSE /\ killed = {}
Begin == /\ phase = "Running" /\ phase' = "Ordering" /\ open' = FALSE
         /\ UNCHANGED <<signalled, finished, index, crashed, reported, released, expired, killed>>
StopNext == /\ phase = "Ordering" /\ index \in Targets
            /\ signalled' = signalled \cup {index} /\ phase' = "Waiting"
            /\ killed' = IF Fault = "KillEarly" THEN killed \cup {index} ELSE killed
            /\ UNCHANGED <<open, finished, index, crashed, reported, released, expired>>
Advance == /\ phase = "Waiting" /\ index \in finished
           /\ index' = index + 1 /\ phase' = "Ordering"
           /\ UNCHANGED <<open, signalled, finished, crashed, reported, released, expired, killed>>
Ordered == /\ phase = "Ordering" /\ index > TargetCount /\ phase' = "Joining"
           /\ UNCHANGED <<open, signalled, finished, index, crashed, reported, released, expired, killed>>
GraceExpired == /\ phase \in {"Ordering", "Waiting"} /\ phase' = "Emergency"
                /\ expired' = TRUE
                /\ UNCHANGED <<open, signalled, finished, index, crashed, reported, released, killed>>
Crash == /\ phase # "Done" /\ ~crashed /\ crashed' = TRUE
         /\ reported' = (Fault # "ForgetFailure") /\ phase' = "Emergency"
         /\ open' = FALSE
         /\ UNCHANGED <<signalled, finished, index, released, expired, killed>>
\* After the grace expired (and no panic took the fallback), every root not
\* yet finished gets the kill; otherwise the parent's Shutdown.
Emergency == /\ phase = "Emergency" /\ phase' = "Joining"
             /\ signalled' = IF Fault = "ForgetSignal" THEN signalled ELSE Targets
             /\ killed' = IF expired /\ ~crashed /\ Fault # "NoKillAtGrace"
                          THEN killed \cup (signalled' \ finished) ELSE killed
             /\ UNCHANGED <<open, finished, index, crashed, reported, released, expired>>
Finish(i) == /\ i \in signalled \ finished /\ finished' = finished \cup {i}
             /\ UNCHANGED <<phase, open, signalled, index, crashed, reported, released, expired, killed>>
Join == /\ phase = "Joining" /\ (finished = Targets \/ Fault = "ReleaseEarly")
        /\ phase' = "Done" /\ released' = TRUE
        /\ UNCHANGED <<open, signalled, finished, index, crashed, reported, expired, killed>>
\* Negative control for the #209 bug: once admission closed, a supervisor
\* startup guard dropped after readiness rolled the (ready) tree back,
\* stopping it outside the coordinator's order. The fix only rolls back
\* unready startups, which are not targets here, so with Fault # "RollbackReady"
\* this action is disabled and the model is unchanged.
RollbackReady(i) == /\ Fault = "RollbackReady" /\ ~open
                    /\ i \in Targets \ signalled /\ signalled' = signalled \cup {i}
                    /\ UNCHANGED <<phase, open, finished, index, crashed, reported, released, expired, killed>>
Next == Begin \/ StopNext \/ Advance \/ Ordered \/ GraceExpired \/ Crash \/ Emergency
        \/ (\E i \in Targets: Finish(i)) \/ Join \/ (\E i \in Targets: RollbackReady(i))
Safety == /\ phase \in {"Running", "Ordering", "Waiting", "Emergency", "Joining", "Done"}
          /\ finished \subseteq signalled /\ signalled \subseteq Targets
          /\ (phase # "Running" => ~open)
          /\ (released => finished = Targets /\ phase = "Done")
          /\ (crashed => reported)
          /\ killed \subseteq signalled
\* Ordered shutdown: while the coordinator is in its ordered loop, exactly
\* the targets before the cursor (and the cursor itself once StopNext ran)
\* have been signalled, and every target before the cursor has finished.
\* So a target is only signalled after every earlier one in stop order
\* finished. (Before Begin nothing is signalled.)
OrderedShutdown ==
    /\ phase = "Running" => signalled = {}
    /\ phase \in {"Ordering", "Waiting"} =>
         /\ \A j \in Targets : j < index => j \in finished
         /\ \A j \in Targets : j \in signalled <=> (j < index \/ (phase = "Waiting" /\ j = index))
\* The coordinator joins (and may release runtime ownership) only after
\* every target was signalled, whether by the ordered loop or the
\* emergency fallback: no root is left running unsignalled behind a join.
JoinAfterAllSignalled == phase \in {"Joining", "Done"} => signalled = Targets
\* v0.8: a root is killed only once the grace expired (before it, roots get
\* the parent's Shutdown and may run stopped()).
KillOnlyAtGrace == killed # {} => expired
\* v0.8: after the grace expired without a panic, the join waits only on
\* killed roots: every root has finished or was killed (so a trapping root
\* stuck in stopped() cannot hold runtime ownership).
GraceKillsRemaining ==
    (phase \in {"Joining", "Done"} /\ expired /\ ~crashed) =>
        \A i \in Targets : i \in finished \/ i \in killed
FairSpec == Init /\ [][Next]_vars
            /\ WF_vars(Begin) /\ WF_vars(StopNext) /\ WF_vars(Advance)
            /\ WF_vars(Ordered) /\ WF_vars(Emergency) /\ WF_vars(Join)
            /\ (\A i \in Targets: WF_vars(Finish(i)))
CleanupCompletes == <> (phase = "Done")
=============================================================================
