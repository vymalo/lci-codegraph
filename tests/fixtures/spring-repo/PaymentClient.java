package com.example.accounts;

import org.springframework.cloud.openfeign.FeignClient;
import org.springframework.web.bind.annotation.PostMapping;

// A declarative HTTP client to a DIFFERENT codebase. The `@PostMapping` below is an *outbound*
// request declaration, not an endpoint this repo serves — so it must NOT become a `route` node.
// That distinction is the whole reason the mapping annotations are read in the context of their
// enclosing type rather than on their own.
@FeignClient(name = "payment-service")
interface PaymentClient {
    @PostMapping("/ledgers")
    void openLedger(String email);
}
