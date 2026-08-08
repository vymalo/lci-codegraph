package com.example.accounts;

import java.util.Optional;

import org.springframework.stereotype.Service;

@Service
class AccountServiceImpl implements AccountService {
    private final AccountRepository repository;

    private final PaymentClient paymentClient;

    AccountServiceImpl(AccountRepository repository, PaymentClient paymentClient) {
        this.repository = repository;
        this.paymentClient = paymentClient;
    }

    // `repository` is a constructor-injected field. Its declared type — not the lowercase variable
    // name — is what singles `AccountRepository.findByEmail` out from `AccountServiceImpl`'s own
    // same-named method.
    @Override
    public Optional<Account> findByEmail(String email) {
        return repository.findByEmail(email);
    }

    // The outbound service boundary: `paymentClient` is a `@FeignClient`, so this call leaves the
    // repo entirely. It resolves to the `external_service` node, not to any symbol in this codebase.
    @Override
    public Account create(String email) {
        paymentClient.openLedger(email);
        return new Account();
    }
}
