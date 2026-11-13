----------------------------- MODULE ReplyTimer -----------------------------
\* Receiver-side refinement of DeferredReplies.Shorten/Expire.
\* The runtime timer and the slot share one exact deadline captured before send.
\* A SHORTENED bit is consumed BEFORE reading the deadline: a concurrent later
\* shortening either appears in that read or leaves another bit to consume.
\* No cached/coarse clock is assumed. Timers can be polled late, never early.
EXTENDS Naturals
CONSTANTS MaxTime, Fault
ASSUME Fault \in {"None", "ClearLate", "ForgetFlag"}
VARIABLES now, deadline, timer, marked, phase, captured, closed
vars == <<now, deadline, timer, marked, phase, captured, closed>>
Init == /\ now = 0 /\ deadline = MaxTime /\ timer = MaxTime
        /\ marked = FALSE /\ phase = "idle" /\ captured = MaxTime /\ closed = FALSE
Shorten == /\ ~closed /\ deadline > 0
           /\ \E earlier \in 0..(deadline-1):
                /\ deadline' = earlier
                /\ marked' = IF Fault = "ForgetFlag" THEN marked ELSE TRUE
           /\ UNCHANGED <<now, timer, phase, captured, closed>>
BeginRearm == /\ ~closed /\ phase = "idle" /\ marked
              /\ phase' = "read"
              /\ marked' = IF Fault = "ClearLate" THEN marked ELSE FALSE
              /\ UNCHANGED <<now, deadline, timer, captured, closed>>
ReadDeadline == /\ ~closed /\ phase = "read"
                /\ captured' = deadline /\ phase' = "install"
                /\ UNCHANGED <<now, deadline, timer, marked, closed>>
InstallTimer == /\ ~closed /\ phase = "install"
                /\ timer' = captured /\ phase' = "idle"
                /\ marked' = IF Fault = "ClearLate" THEN FALSE ELSE marked
                /\ UNCHANGED <<now, deadline, captured, closed>>
Tick == /\ now < MaxTime /\ now' = now + 1
        /\ UNCHANGED <<deadline, timer, marked, phase, captured, closed>>
Expire == /\ ~closed /\ now >= timer /\ closed' = TRUE
          /\ UNCHANGED <<now, deadline, timer, marked, phase, captured>>
Done == /\ closed /\ now = MaxTime /\ UNCHANGED vars
Next == Shorten \/ BeginRearm \/ ReadDeadline \/ InstallTimer \/ Tick \/ Expire \/ Done
Spec == Init /\ [][Next]_vars
FairSpec == Spec /\ WF_vars(Tick) /\ WF_vars(BeginRearm)
                 /\ WF_vars(ReadDeadline) /\ WF_vars(InstallTimer) /\ WF_vars(Expire)
TypeOK == /\ now \in 0..MaxTime /\ deadline \in 0..MaxTime
          /\ timer \in 0..MaxTime /\ captured \in 0..MaxTime
          /\ marked \in BOOLEAN /\ closed \in BOOLEAN
          /\ phase \in {"idle", "read", "install"}
NeverEarly == closed => now >= deadline
NoLostRearm == (~closed /\ phase = "idle" /\ ~marked) => timer = deadline
EventuallyExpires == <>closed
=============================================================================
