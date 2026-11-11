------------------------ MODULE MCSupervisionTemporary ------------------------
(* Per-PR instance with a Temporary child, so TemporaryStartsOnce is not
   vacuous (MCSupervision has only Permanent and Transient children). *)
EXTENDS Supervision

MCModeTemp == <<"Temporary", "Permanent">>
=============================================================================
