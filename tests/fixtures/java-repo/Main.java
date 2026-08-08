class Main {
    void run() {
        Util.caller();
        Widget.build();
    }

    void runBare() {
        // `build` is defined on both Widget and Gadget with no qualifier here — must be
        // dropped as ambiguous, never guessed.
        build();
    }

    // Instance call through a variable receiver (issue #8): `spin` has exactly one definition
    // in the repo (Gizmo.spin()), so this must resolve even though the variable is named `g`,
    // not `Gizmo` — a value receiver carries no type information, and the call falls through to
    // bare-name resolution, exactly like a bare `spin()` call would.
    void runViaVariable() {
        Gizmo g = new Gizmo();
        g.spin();
    }

    // Two same-named methods exist (Lefty.orbit()/Righty.orbit()), but `l`'s DECLARED type is
    // right there on the previous line, so this is not ambiguous at all — it must resolve to
    // Lefty.orbit specifically, never Righty.orbit. Declared-type recovery is what makes a
    // multi-candidate set resolvable; issue #8's lowercase-receiver rule alone would drop this,
    // because dropping the qualifier leaves nothing to choose with.
    void runDisambiguatedByDeclaredType() {
        Lefty l = new Lefty();
        l.orbit();
    }

    // Negative: the receiver is typed as the INTERFACE both classes implement, so both are
    // genuine candidates and the qualifier matches both through their `implements` clause. There
    // is nothing to choose between them, and the resolver must drop the call rather than guess —
    // this is `@Primary`/`@Qualifier` territory, deliberately out of scope.
    void runAmbiguousViaInterface(Orbiter o) {
        o.orbit();
    }
}
