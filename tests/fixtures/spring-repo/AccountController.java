package com.example.accounts;

import java.util.Optional;

import org.springframework.web.bind.annotation.GetMapping;
import org.springframework.web.bind.annotation.PathVariable;
import org.springframework.web.bind.annotation.PostMapping;
import org.springframework.web.bind.annotation.RequestBody;
import org.springframework.web.bind.annotation.RequestMapping;
import org.springframework.web.bind.annotation.RestController;

// The class-level prefix composes with each method-level suffix, so the routes this file serves are
// `GET /api/accounts/{email}` and `POST /api/accounts` — neither of which appears as a literal
// string anywhere in the source a reviewer would grep.
@RestController
@RequestMapping("/api/accounts")
class AccountController {
    private final AccountService accountService;

    AccountController(AccountService accountService) {
        this.accountService = accountService;
    }

    @GetMapping("/{email}")
    Optional<Account> getAccount(@PathVariable String email) {
        return accountService.findByEmail(email);
    }

    // A bare `@PostMapping` with no path argument: the route is the class-level prefix alone.
    @PostMapping
    Account createAccount(@RequestBody String email) {
        return accountService.create(email);
    }
}
