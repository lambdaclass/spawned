------------------------ MODULE NestedSupervision ------------------------
(* Ownership projection. Actor handlers, stop-turn scheduling and local    *)
(* restart policy are checked by the other models, not duplicated here.   *)
EXTENDS Naturals, FiniteSets, TLC
(* `@type:` comments are Apalache annotations (NestedSupervisionInd.tla);  *)
(* TLC ignores them. `c \in 1..s.arity[n]` is written `1 <= c /\ c <=     *)
(* s.arity[n]` (same predicate for integers; Apalache needs constant      *)
(* range bounds).                                                          *)
CONSTANTS
 \* @type: Int;
 NodeCount,
 \* @type: Int;
 Slots,
 \* @type: Int;
 MaxGen,
 \* @type: Str;
 Fault
Nodes == 1..NodeCount
Children == 1..Slots
\* @typeAlias: nestedState = { nodes: Set(Int), started: Set(Int),
\*   closed: Set(Int), cancelled: Set(Int), done: Set(Int),
\*   propagated: Set(Int), arity: Int -> Int, parent: Int -> Int,
\*   slot: Int -> Int, owner: Int -> Int, gen: Int -> (Int -> Int),
\*   live: Int -> (Int -> Set(Int)), badJoin: Bool, badStart: Bool };
NestedTypeAliases == TRUE
VARIABLE
 \* @type: $nestedState;
 s
\* @type: <<$nestedState>>;
vars == <<s>>
Init ==
 s = [nodes |-> {1}, started |-> {}, closed |-> {}, cancelled |-> {},
      done |-> {}, propagated |-> {},
      arity |-> [n \in Nodes |-> 0],
      parent |-> [n \in Nodes |-> 0], slot |-> [n \in Nodes |-> 0],
      owner |-> [n \in Nodes |-> 0],
      gen |-> [n \in Nodes |-> [c \in Children |-> 0]],
      live |-> [n \in Nodes |-> [c \in Children |-> {}]],
      badJoin |-> FALSE, badStart |-> FALSE]
Descendants(n) == {q \in s.nodes : s.parent[q] = n}
Owned(n,c,g) == {q \in s.nodes : s.parent[q] = n /\ s.slot[q] = c /\ s.owner[q] = g}
Empty(n) == \A c \in Children : s.live[n][c] = {}
TCreate(n,a) ==
 /\ n \in s.nodes /\ n \notin s.started /\ a \in 0..Slots
 /\ n = 1 \/ a = s.arity[n]
 /\ s' = [s EXCEPT !.started = @ \cup {n}, !.arity[n] = a]
TBegin(n,c,g) ==
 /\ n \in s.started /\ 1 <= c /\ c <= s.arity[n] /\ n \notin s.done
 /\ n \notin s.closed \/ Fault = "Closed"
 /\ g = s.gen[n][c] + 1 /\ g <= MaxGen /\ s.live[n][c] = {}
 /\ s' = [s EXCEPT !.gen[n][c] = g, !.live[n][c] = {g},
          !.badStart = @ \/ n \in s.closed]
TAttach(n,c,g,q,a) ==
 /\ n \in s.nodes /\ q \in Nodes \ s.nodes /\ a \in 0..Slots
 /\ 1 <= c /\ c <= s.arity[n] /\ g \in s.live[n][c]
 /\ s' = [s EXCEPT !.nodes = @ \cup {q}, !.arity[q] = a,
          !.parent[q] = n, !.slot[q] = c, !.owner[q] = g]
TJoined(n,c,g) ==
 /\ n \in s.nodes /\ c \in Children /\ g \in s.live[n][c]
 /\ Owned(n,c,g) \subseteq s.done \/ Fault = "Join"
 /\ s' = [s EXCEPT !.live[n][c] = @ \ {g},
          !.badJoin = @ \/ ~(Owned(n,c,g) \subseteq s.done)]
TClose(n) ==
 /\ n \in s.nodes /\ n \notin s.closed
 /\ s' = [s EXCEPT !.closed = @ \cup {n}]
TCancel(n) ==
 /\ n \in s.closed /\ n \notin s.cancelled
 /\ s' = [s EXCEPT !.cancelled = @ \cup {n}]
TPropagate(n,x) ==
 /\ n \in s.nodes /\ s.parent[n] # 0 /\ n \in s.cancelled /\ x = TRUE
 /\ s' = [s EXCEPT !.propagated = @ \cup {n}]
TDone(n) ==
 /\ n \in s.closed /\ n \notin s.done /\ Empty(n)
 /\ Descendants(n) \subseteq s.done
 /\ s' = [s EXCEPT !.done = @ \cup {n}]
(* Progress assumptions, not preemption: cancellation is eventually        *)
(* delivered and each cancelled generation eventually drops its leases.   *)
Deliver(n) ==
 /\ Fault # "Propagation" /\ n \in s.nodes /\ s.parent[n] \in s.cancelled
 /\ n \notin s.cancelled
 /\ s' = [s EXCEPT !.closed = @ \cup {n}, !.cancelled = @ \cup {n}]
Cleanup(n,c,g) == /\ n \in s.cancelled /\ TJoined(n,c,g)
TypeOK ==
 /\ s.nodes \subseteq Nodes /\ 1 \in s.nodes
 /\ s.started \subseteq s.nodes /\ s.closed \subseteq s.nodes
 /\ s.cancelled \subseteq s.closed /\ s.done \subseteq s.closed
 /\ s.parent \in [Nodes -> 0..NodeCount]
 /\ s.arity \in [Nodes -> 0..Slots]
 /\ s.gen \in [Nodes -> [Children -> 0..MaxGen]]
 /\ s.live \in [Nodes -> [Children -> SUBSET (1..MaxGen)]]
NoEarlyParentJoin == ~s.badJoin
NoStartAfterClose == ~s.badStart
DoneMeansDescendantsJoined ==
 \A n \in s.done : Empty(n) /\ Descendants(n) \subseteq s.done
AtMostOneGeneration == \A n \in Nodes, c \in Children : Cardinality(s.live[n][c]) <= 1
Safety == TypeOK /\ NoEarlyParentJoin /\ NoStartAfterClose
          /\ DoneMeansDescendantsJoined /\ AtMostOneGeneration
Next ==
 \/ \E n \in Nodes, a \in 0..Slots : TCreate(n,a)
 \/ \E n \in Nodes, c \in Children, g \in 1..MaxGen : TBegin(n,c,g) \/ TJoined(n,c,g)
 \/ \E n,q \in Nodes, c \in Children, g \in 1..MaxGen, a \in 0..Slots : TAttach(n,c,g,q,a)
 \/ \E n \in Nodes : TClose(n) \/ TCancel(n) \/ TPropagate(n,TRUE) \/ TDone(n) \/ Deliver(n)
Spec == Init /\ [][Next]_vars
FairSpec == Spec
 /\ (\A n \in Nodes : WF_vars(Deliver(n)) /\ WF_vars(TDone(n)))
 /\ (\A n \in Nodes,c \in Children,g \in 1..MaxGen : WF_vars(Cleanup(n,c,g)))
ClosedLeadsToDone == (1 \in s.cancelled) ~> (1 \in s.done)
=============================================================================
