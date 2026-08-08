package com.example.accounts;

import jakarta.persistence.Entity;
import jakarta.persistence.Id;

// A plain JPA entity. Deliberately produces NO Spring-specific graph facts: `docs/design/
// spring-aware-graph.md` §2.4 rejects a `persists` relation, because the one thing it would say
// ("AccountRepository manages Account") is already spelled out in the repository's own `extends
// JpaRepository<Account, Long>` clause. This file is here to prove the extractor stays silent.
@Entity
class Account {
    @Id
    private Long id;

    private String email;

    Long getId() {
        return id;
    }

    String getEmail() {
        return email;
    }
}
