      - [guard checker does not account for cross calls to compute uniform arguments of a nested mutual fixpoint](#guard-checker-does-not-account-for-cross-calls-to-compute-uniform-arguments-of-a-nested-mutual-fixpoint)
      - [guard checker does not check for correct recursive calls when passed as uniform argument in a nested fixpoint](#guard-checker-does-not-check-for-correct-recursive-calls-when-passed-as-uniform-argument-in-a-nested-fixpoint)
      - [guard checker does not count argument-less recursive calls to compute uniform arguments of a nested mutual fixpoint](#guard-checker-does-not-count-argument-less-recursive-calls-to-compute-uniform-arguments-of-a-nested-mutual-fixpoint)
    - [Module system](#module-system)
      - [missing universe constraints in typing "with" clause of a module type](#missing-universe-constraints-in-typing-with-clause-of-a-module-type)
      - [universe constraints for module subtyping not stored in vo files](#universe-constraints-for-module-subtyping-not-stored-in-vo-files)
- exploit / GH issue: [#21701](https://github.com/rocq-prover/rocq/issues/21701)
- risk: unknown (no development in CI was affected)

### Module system

#### missing universe constraints in typing "with" clause of a module type
