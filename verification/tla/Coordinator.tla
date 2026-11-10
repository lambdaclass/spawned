-------------------------- MODULE Coordinator --------------------------
EXTENDS Naturals, FiniteSets
(* The ActorSystem shutdown coordinator (begin_shutdown in                  *)
(* concurrency/src/system.rs). Targets are the admitted roots in STOP      *)
(* order: target 1 is the last-admitted root (the Rust loop walks          *)
(* `targets.iter().rev()`), so "lower index stops first" is "later         *)
(* admitted stops first". Targets abstract roots whose startup committed;  *)
(* an unready supervisor startup rolls itself back and is not modelled.    *)
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
    released
vars == <<phase, open, signalled, finished, index, crashed, reported, released>>

Init == /\ phase = "Running" /\ open = TRUE
        /\ signalled = {} /\ finished = {} /\ index = 1
        /\ crashed = FALSE /\ reported = FALSE /\ released = FALSE
Begin == /\ phase = "Running" /\ phase' = "Ordering" /\ open' = FALSE
         /\ UNCHANGED <<signalled, finished, index, crashed, reported, released>>
StopNext == /\ phase = "Ordering" /\ index \in Targets
            /\ signalled' = signalled \cup {index} /\ phase' = "Waiting"
            /\ UNCHANGED <<open, finished, index, crashed, reported, released>>
Advance == /\ phase = "Waiting" /\ index \in finished
           /\ index' = index + 1 /\ phase' = "Ordering"
           /\ UNCHANGED <<open, signalled, finished, crashed, reported, released>>
Ordered == /\ phase = "Ordering" /\ index > TargetCount /\ phase' = "Joining"
           /\ UNCHANGED <<open, signalled, finished, index, crashed, reported, released>>
GraceExpired == /\ phase \in {"Ordering", "Waiting"} /\ phase' = "Emergency"
                /\ UNCHANGED <<open, signalled, finished, index, crashed, reported, released>>
Crash == /\ phase # "Done" /\ ~crashed /\ crashed' = TRUE
         /\ reported' = (Fault # "ForgetFailure") /\ phase' = "Emergency"
         /\ open' = FALSE
         /\ UNCHANGED <<signalled, finished, index, released>>
Emergency == /\ phase = "Emergency" /\ phase' = "Joining"
             /\ signalled' = IF Fault = "ForgetSignal" THEN signalled ELSE Targets
             /\ UNCHANGED <<open, finished, index, crashed, reported, released>>
Finish(i) == /\ i \in signalled \ finished /\ finished' = finished \cup {i}
             /\ UNCHANGED <<phase, open, signalled, index, crashed, reported, released>>
Join == /\ phase = "Joining" /\ (finished = Targets \/ Fault = "ReleaseEarly")
        /\ phase' = "Done" /\ released' = TRUE
        /\ UNCHANGED <<open, signalled, finished, index, crashed, reported>>
\* Negative control for the #209 bug: once admission closed, a supervisor
\* startup guard dropped after readiness rolled the (ready) tree back,
\* stopping it outside the coordinator's order. The fix only rolls back
\* unready startups, which are not targets here, so with Fault # "RollbackReady"
\* this action is disabled and the model is unchanged.
RollbackReady(i) == /\ Fault = "RollbackReady" /\ ~open
                    /\ i \in Targets \ signalled /\ signalled' = signalled \cup {i}
                    /\ UNCHANGED <<phase, open, finished, index, crashed, reported, released>>
Next == Begin \/ StopNext \/ Advance \/ Ordered \/ GraceExpired \/ Crash \/ Emergency
        \/ (\E i \in Targets: Finish(i)) \/ Join \/ (\E i \in Targets: RollbackReady(i))
Safety == /\ phase \in {"Running", "Ordering", "Waiting", "Emergency", "Joining", "Done"}
          /\ finished \subseteq signalled /\ signalled \subseteq Targets
          /\ (phase # "Running" => ~open)
          /\ (released => finished = Targets /\ phase = "Done")
          /\ (crashed => reported)
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
FairSpec == Init /\ [][Next]_vars
            /\ WF_vars(Begin) /\ WF_vars(StopNext) /\ WF_vars(Advance)
            /\ WF_vars(Ordered) /\ WF_vars(Emergency) /\ WF_vars(Join)
            /\ (\A i \in Targets: WF_vars(Finish(i)))
CleanupCompletes == <> (phase = "Done")
=============================================================================
