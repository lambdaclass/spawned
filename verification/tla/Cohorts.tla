----------------------------- MODULE Cohorts -----------------------------
(* Coordinator policy and actual generation ownership. Backoff durations, *)
(* handlers and retry budgets are abstracted; Supervision covers policy.   *)
EXTENDS Naturals, FiniteSets, Sequences, TLC
(* `@type:` comments are Apalache annotations (CohortsInd.tla); TLC       *)
(* ignores them. Integer ranges with a state-dependent bound are written   *)
(* as comparisons (`1 <= r /\ r <= s.epoch` for `r \in 1..s.epoch`), which *)
(* is the same predicate for integers and lets Apalache handle them.       *)
CONSTANTS
 \* @type: Int;
 N,
 \* @type: Int -> Str;
 Mode,
 \* @type: Str;
 Strategy,
 \* @type: Int;
 MaxGen,
 \* @type: Int;
 MaxRounds,
 \* @type: Str;
 Fault
Children == 1..N
Epochs == 0..MaxRounds
ExpectedStart(c) == IF Strategy = "OneForAll" THEN 1 ELSE c
Minimum(xs) == CHOOSE x \in xs : \A y \in xs : x <= y
Maximum(xs) == CHOOSE x \in xs : \A y \in xs : x >= y
ASSUME Strategy \in {"OneForAll", "RestForOne"}
ASSUME Mode \in [Children -> {"Permanent", "Transient", "Temporary"}]
\* @typeAlias: cohortState = { open: Bool, initial: Set(Int),
\*   committed: Bool, phase: Str, epoch: Int, origin: Int, start: Int,
\*   pending: Set(Int), covered: Int -> Int, assigned: Int -> Int,
\*   scanned: Set(Int), affected: Set(Int), stopped: Set(Int),
\*   clean: Set(Int), resumed: Set(Int), ack: Set(Int), terminal: Set(Int),
\*   gen: Int -> Int, live: Int -> Set(Int), published: Int -> Int,
\*   badBarrier: Bool, badClosed: Bool, badTerminal: Bool };
CohortTypeAliases == TRUE
VARIABLE
 \* @type: $cohortState;
 s
\* @type: <<$cohortState>>;
vars == <<s>>
Init ==
 s = [open |-> TRUE, initial |-> {}, committed |-> FALSE,
      phase |-> "Idle", epoch |-> 0, origin |-> 0, start |-> 0,
      pending |-> {}, covered |-> [c \in Children |-> 0],
      assigned |-> [c \in Children |-> 0], scanned |-> {},
      affected |-> {}, stopped |-> {}, clean |-> {}, resumed |-> {},
      ack |-> {}, terminal |-> {}, gen |-> [c \in Children |-> 0],
      live |-> [c \in Children |-> {}], published |-> [c \in Children |-> 0],
      badBarrier |-> FALSE, badClosed |-> FALSE, badTerminal |-> FALSE]
Range == {j \in 0..N : s.start <= j}   \* = s.start..N, since start \in 0..N
Active == Children \ s.terminal
CRequest(c,r,x) ==
 /\ s.open /\ s.committed /\ c \in Children /\ 0 <= r /\ r <= s.assigned[c]
 /\ s.gen[c] > 0 /\ s.live[c] = {} /\ Mode[c] # "Temporary"
 /\ x = IF r >= s.covered[c] THEN "true" ELSE "false"
 /\ s' = [s EXCEPT !.pending = IF x = "true" THEN @ \cup {c} ELSE @]
CSelect(c,r,n) ==
 /\ s.phase \in {"Idle","Starting"}
 /\ s.pending # {} /\ c = Minimum(s.pending)
 /\ r = s.epoch + 1 /\ r <= MaxRounds
 /\ n = IF Fault = "Selection" THEN c ELSE ExpectedStart(c)
 /\ s' = [s EXCEPT !.epoch = r, !.origin = c, !.start = n,
          !.covered = [j \in Children |-> IF j >= n THEN r ELSE s.covered[j]],
          !.pending = {j \in s.pending : j < n},
          !.scanned = {}, !.affected = {}, !.stopped = {}, !.clean = {},
          !.resumed = {}, !.ack = {}, !.phase = "Assign"]
CAssign(c,r,x) ==
 /\ s.phase = "Assign" /\ r = s.epoch
 /\ Range \ s.scanned # {} /\ c = Minimum(Range \ s.scanned)
 /\ x = IF c \in Active \/ Fault = "Retirement" THEN "true" ELSE "false"
 /\ s' = [s EXCEPT !.scanned = @ \cup {c},
          !.affected = IF x = "true" THEN @ \cup {c} ELSE @,
          !.assigned[c] = IF x = "true" THEN r ELSE @,
          !.badTerminal = @ \/ (x = "true" /\ c \in s.terminal),
          !.phase = IF s.scanned \cup {c} = Range THEN "Stopping" ELSE "Assign"]
CStop(c,r) ==
 /\ s.phase = "Stopping" /\ r = s.epoch
 /\ s.affected \ s.clean # {} /\ c = Maximum(s.affected \ s.clean)
 /\ c \notin s.stopped
 /\ s' = [s EXCEPT !.stopped = @ \cup {c}]
CClean(c,r) ==
 /\ c \in Children /\ 1 <= r /\ r <= s.epoch
 /\ IF r < s.epoch THEN s' = s
    ELSE /\ c \in s.stopped /\ s.live[c] = {}
         /\ s' = [s EXCEPT !.clean = @ \cup {c}]
CResume(c,r) ==
 /\ s.phase \in {"Stopping","Starting"} /\ r = s.epoch
 /\ s.clean = s.affected \/ Fault = "Cleanup"
 /\ s.affected \ s.resumed # {} /\ c = Minimum(s.affected \ s.resumed)
 /\ \A j \in s.affected : j < c => j \in s.ack
 /\ s' = [s EXCEPT !.resumed = @ \cup {c}, !.phase = "Starting"]
CBegin(c,r,n) ==
 /\ c \in Active /\ n = s.gen[c] + 1 /\ n <= MaxGen
 /\ s.open \/ Fault = "Closed"
 /\ \/ /\ r = 0 /\ ~s.committed /\ c \notin s.initial
       /\ \A j \in Children : j < c => j \in s.initial
    \/ /\ r = s.assigned[c] /\ r > 0 /\ c \in s.resumed /\ c \notin s.ack
 /\ s.live[c] = {} \/ Fault = "Cleanup"
 /\ Mode[c] # "Temporary" \/ s.gen[c] = 0
 /\ s' = [s EXCEPT !.gen[c] = n, !.live[c] = @ \cup {n},
          !.badBarrier = @ \/ (r > 0 /\ s.clean # s.affected),
          !.badClosed = @ \/ ~s.open]
CReady(c,r,n) ==
 /\ c \in Children /\ n = s.gen[c] /\ n \in s.live[c]
 /\ r = s.assigned[c] /\ s.open
 /\ r = 0 \/ c \in s.resumed
 /\ s' = [s EXCEPT !.initial = @ \cup {c}, !.published[c] = n]
CJoined(c,n) ==
 /\ c \in Children /\ n \in s.live[c]
 /\ s' = [s EXCEPT !.live[c] = @ \ {n},
          !.published[c] = IF @ = n THEN 0 ELSE @]
CRetire(c,r) ==
 /\ c \in Children /\ s.live[c] = {}
 /\ ~s.open \/ r = s.assigned[c]
 /\ s' = [s EXCEPT !.terminal = @ \cup {c}]
CAck(c,r) ==
 /\ c \in Children /\ 1 <= r /\ r <= s.epoch
 /\ IF r < s.epoch THEN s' = s
    ELSE /\ c \in s.resumed
         /\ (s.published[c] = s.gen[c] /\ s.gen[c] > 0) \/ c \in s.terminal
         /\ s' = [s EXCEPT !.ack = @ \cup {c}]
CEnd(r,x) ==
 /\ r = s.epoch /\ s.phase \in {"Stopping","Starting"}
 /\ \/ /\ x = "superseded" /\ s.pending # {}
    \/ /\ x = "complete" /\ s.ack = s.affected
 /\ s' = [s EXCEPT !.phase = "Idle"]
CInitialReady ==
 /\ s.open /\ s.initial = Children /\ ~s.committed
 /\ s' = [s EXCEPT !.committed = TRUE]
CClose == /\ s.open /\ s' = [s EXCEPT !.open = FALSE]
CDone ==
 /\ ~s.open /\ \A c \in Children : s.live[c] = {}
 /\ s' = [s EXCEPT !.phase = "Done"]
TypeOK ==
 /\ s.open \in BOOLEAN /\ s.committed \in BOOLEAN
 /\ s.phase \in {"Idle","Assign","Stopping","Starting","Done"}
 /\ s.pending \subseteq Children /\ s.terminal \subseteq Children
 /\ s.initial \subseteq Children /\ s.affected \subseteq Children
 /\ s.stopped \subseteq Children /\ s.clean \subseteq Children
 /\ s.resumed \subseteq Children /\ s.ack \subseteq Children
 /\ s.epoch \in Epochs /\ s.covered \in [Children -> Epochs]
 /\ s.assigned \in [Children -> Epochs]
 /\ s.gen \in [Children -> 0..MaxGen]
 /\ s.live \in [Children -> SUBSET (1..MaxGen)]
 /\ s.published \in [Children -> 0..MaxGen]
AtMostOneGeneration == \A c \in Children : Cardinality(s.live[c]) <= 1
PublishedIsLive == \A c \in Children : s.published[c] # 0 => s.published[c] \in s.live[c]
StrategySelection == s.epoch = 0 \/ s.start = ExpectedStart(s.origin)
WholeCohortBarrier == ~s.badBarrier
NoStartAfterClose == ~s.badClosed
NoRetiredParticipant == ~s.badTerminal
TemporaryStartsOnce == \A c \in Children : Mode[c] = "Temporary" => s.gen[c] <= 1
DoneMeansQuiescent == s.phase = "Done" => \A c \in Children : s.live[c] = {}
Safety == TypeOK /\ AtMostOneGeneration /\ PublishedIsLive /\ StrategySelection
          /\ WholeCohortBarrier /\ NoStartAfterClose /\ NoRetiredParticipant
          /\ TemporaryStartsOnce /\ DoneMeansQuiescent
Next ==
 \/ \E c \in Children, r \in Epochs, x \in {"true","false"} : CRequest(c,r,x)
 \/ \E c \in Children, r \in Epochs, n \in Children : CSelect(c,r,n)
 \/ \E c \in Children, r \in Epochs, x \in {"true","false"} : CAssign(c,r,x)
 \/ \E c \in Children, r \in Epochs : CStop(c,r) \/ CClean(c,r) \/ CResume(c,r) \/ CAck(c,r) \/ CRetire(c,r)
 \/ \E c \in Children, r \in Epochs, n \in 1..MaxGen : CBegin(c,r,n) \/ CReady(c,r,n)
 \/ \E c \in Children, n \in 1..MaxGen : CJoined(c,n)
 \/ \E r \in Epochs, x \in {"complete","superseded"} : CEnd(r,x)
 \/ CInitialReady \/ CClose \/ CDone
Spec == Init /\ [][Next]_vars
=============================================================================
