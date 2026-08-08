package com.example.accounts;

import java.util.Optional;

import org.springframework.data.jpa.repository.JpaRepository;
import org.springframework.stereotype.Repository;

@Repository
interface AccountRepository extends JpaRepository<Account, Long> {
    // A Spring Data derived query: the method NAME is the whole specification, and Spring generates
    // the implementation at runtime. There is no body anywhere in this repo — not "elsewhere",
    // nowhere. The plain declaration-exclusion rule (issue #5) would make this a permanently
    // unreachable node, so §4.3's carve-out keeps a bodiless method on a Spring Data marker
    // interface as a valid terminal call target.
    Optional<Account> findByEmail(String email);
}
