-------------------------- MODULE Coordinator --------------------------
EXTENDS Naturals, FiniteSets
CONSTANTS TargetCount, Fault
Targets == 1..TargetCount
VARIABLES phase, open, signalled, finished, index, crashed, reported, released
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
Next == Begin \/ StopNext \/ Advance \/ Ordered \/ GraceExpired \/ Crash \/ Emergency
        \/ (\E i \in Targets: Finish(i)) \/ Join
Safety == /\ phase \in {"Running", "Ordering", "Waiting", "Emergency", "Joining", "Done"}
          /\ finished \subseteq signalled /\ signalled \subseteq Targets
          /\ (phase # "Running" => ~open)
          /\ (released => finished = Targets /\ phase = "Done")
          /\ (crashed => reported)
FairSpec == Init /\ [][Next]_vars
            /\ WF_vars(Begin) /\ WF_vars(StopNext) /\ WF_vars(Advance)
            /\ WF_vars(Ordered) /\ WF_vars(Emergency) /\ WF_vars(Join)
            /\ (\A i \in Targets: WF_vars(Finish(i)))
CleanupCompletes == <> (phase = "Done")
=============================================================================
