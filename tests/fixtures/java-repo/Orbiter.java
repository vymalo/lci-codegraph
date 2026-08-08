// The interface Lefty and Righty both implement, so that `java-repo` keeps a call that is
// GENUINELY ambiguous even under declared-type qualifier recovery.
//
// Before that recovery landed, `Lefty l = new Lefty(); l.orbit();` was ambiguous for a boring
// reason: the resolver could not tell what `l` was. It can now — `Lefty` is declared one line
// above — so that call resolves, correctly, to `Lefty.orbit`. An interface-typed receiver is what
// stays ambiguous for a REAL reason: `Orbiter.orbit` is bodiless (so it is not a call target), and
// both implementations match the qualifier through their `implements` clause, leaving two genuine
// candidates and nothing to choose between them. That is the case the precision policy exists for,
// and it must still be dropped rather than guessed.
interface Orbiter {
    int orbit();
}
