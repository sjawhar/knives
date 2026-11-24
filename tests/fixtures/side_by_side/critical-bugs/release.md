      - [guard checker does not account for cross calls to compute uniform arguments of a nested mutual fixpoint](#guard-checker-does-not-account-for-cross-calls-to-compute-uniform-arguments-of-a-nested-mutual-fixpoint)
      - [guard checker does not check for correct recursive calls when passed as uniform argument in a nested fixpoint](#guard-checker-does-not-check-for-correct-recursive-calls-when-passed-as-uniform-argument-in-a-nested-fixpoint)
      - [guard checker does not count argument-less recursive calls to compute uniform arguments of a nested mutual fixpoint](#guard-checker-does-not-count-argument-less-recursive-calls-to-compute-uniform-arguments-of-a-nested-mutual-fixpoint)
      - [incorrect variance inference for a constructor applied in a letin of a constructor type](#incorrect-variance-inference-for-a-constructor-applied-in-a-letin-of-a-constructor-type)
      - [incorrect reification in lazy machine of matches with universe polymorphism](#incorrect-reification-in-lazy-machine-of-matches-with-universe-polymorphism)
      - [guard checker computes commutative cuts in the wrong environment and breaks propositional extensionality](#guard-checker-computes-commutative-cuts-in-the-wrong-environment-and-breaks-propositional-extensionality)
    - [Section system](#section-system)
      - [projections are cooked in wrong environment](#projections-are-cooked-in-wrong-environment)
    - [Module system](#module-system)
      - [missing universe constraints in typing "with" clause of a module type](#missing-universe-constraints-in-typing-with-clause-of-a-module-type)
      - [universe constraints for module subtyping not stored in vo files](#universe-constraints-for-module-subtyping-not-stored-in-vo-files)
- exploit / GH issue: [#21701](https://github.com/rocq-prover/rocq/issues/21701)
- risk: unknown (no development in CI was affected)

#### incorrect variance inference with letin in constructor type
- component: inductive declarations, universe polymorphism
- introduced: V8.6 (cumulative inductives)
- impacted released versions: V8.6 to V9.2.0
- impacted rocqchk versions: same
- fixed in: V9.2.1, V9.3
- found by: OpenAI
- issue: #22385
- risk: the bug comes from being able to bind a constructor type letin in a match,
  which means those letins cannot be irrelevant for conversion.
  Differences in reduction may mean the bug is harder to exploit before V8.19, see issue.

#### incorrect variance inference for a constructor applied in a letin of a constructor type
- component: inductive declarations, universe polymorphism
- introduced: V9.3.0 ([#22385](https://github.com/rocq-prover/rocq/pull/22385))
- impacted released versions: V9.3.0
- impacted coqchk versions: Same
- fixed in: V9.2.1, V9.3.1, V9.4 ([#22681](https://github.com/rocq-prover/rocq/pull/22681))
- found by: Sami Jawhar
- exploit: test-suite/bugs/bug_22675.v
- GH issue number: [#22675](https://github.com/rocq-prover/rocq/issues/22675)
- risk: needs a cumulative inductive one of whose constructors has a letin whose
  body contains a fully applied constructor of another cumulative inductive
  which has a letin of its own. V9.2.0 accepts the issue's file through
  [#22383](https://github.com/rocq-prover/rocq/issues/22383): it does not infer
  letin bodies at all.

#### incorrect reification in lazy machine of matches with universe polymorphism
- component: "lazy" reduction
- introduced: V8.17
- impacted released versions: V8.17 to V9.2.0
- impacted rocqchk versions: same
- fixed in: V9.2.1, V9.3
- found by: OpenAI
- issue: #22380
- risk: reification of the lazy machine is only trusted
  when it is used to check an application or match/proj
  (typically in `f x`, the inferred type of `f` is reduced to expose a `forall`).
  The bug occurs when the reduced type has a stuck match on a universe polymorphic inductive.
  The exploit in the issue uses Definitional UIP but the code is incorrect even without that flag.

#### guard checker computes commutative cuts in the wrong environment and breaks propositional extensionality
- component: guard checking
- introduced: 8.8 ([#920](https://github.com/rocq-prover/rocq/pull/920))
- impacted released versions: from 8.8 although inconsistency with propext was achievable in other ways
- impacted coqchk versions: Same
- fixed in: 9.2.1, 9.3.1, 9.4 ([#22763](https://github.com/rocq-prover/rocq/pull/22763))
- found by: Pierre-Marie Pédrot
- exploit: test-suite/test-suite/bugs/bug_guard_filter_stack_env.v
- GH issue number: none
- risk: requires the use of propext

### Section system

#### projections are cooked in wrong environment

- component: sections, projections
- introduced: V8.12 (inductive cooking doesn't recheck it)
- impacted released versions: V8.12 to V9.3.0
- impacted coqchk versions: none
- fixed in: V9.2.1, V9.3.1, v9.4 [#22767](https://github.com/rocq-prover/rocq/issues/22767)
- found by: Axel Mierczuk, Spencer Michaels
- exploit: test-suite/bugs/bug_22761.v
- GH issue number: [#22761](https://github.com/rocq-prover/rocq/issues/22761)
- risk: needs local definition in record parameters, in a section with variables

### Module system

#### missing universe constraints in typing "with" clause of a module type
