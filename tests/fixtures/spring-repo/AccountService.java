package com.example.accounts;

import java.util.Optional;

// The interface everything upstream is typed against. Note `findByEmail` deliberately shares its
// name with `AccountRepository.findByEmail`: two same-named candidates that only the receiver's
// DECLARED TYPE can tell apart (§4.2), which is exactly the case the resolver has to get right.
interface AccountService {
    Optional<Account> findByEmail(String email);

    Account create(String email);
}
