# Licence FAQ

Workforce is **source-available** under the [Elastic License 2.0](LICENSE)
(ELv2). This page explains, in plain terms, what that allows. It is a guide,
not part of the licence: if anything here differs from [LICENSE](LICENSE), the
licence wins.

## The short version

You may use, copy, modify and redistribute Workforce, for free, for any
purpose — including commercial use inside your own organisation. You may
**not** offer Workforce to third parties as a hosted or managed service.

## Can I…

| Use | Allowed? |
|---|---|
| Run Workforce for myself, my team or my whole company | Yes |
| Run it inside a commercial company, for that company's own staff | Yes |
| Modify it and run my modified version for my organisation | Yes |
| Build my own product that uses Workforce behind the scenes, where my users never get Workforce's own features | Yes |
| Install and configure it on a client's own infrastructure, for that client's own use, as a paid contractor | Yes |
| Redistribute it (modified or not), keeping the licence and notices | Yes |
| Sell hosted access to Workforce, or to a product whose value is Workforce's features (a "Workforce cloud", a white-labelled agent platform, a multi-tenant SaaS built on it) | **No** |
| Run it on my own servers and let customers use its features as a service, paid or free | **No** |
| Remove or obscure the licence, copyright or other notices | **No** |

If you want to do something in the "No" rows, ask for a commercial licence.

## Is this open source?

Not in the OSI sense. OSI open-source licences may not restrict how the
software is used, and ELv2 restricts one use: offering it as a managed
service. That is why we call Workforce *source-available*.

## What about the skill SDK?

`crates/wasm-skill-sdk` — the crate third-party skills compile against — is
under the [MIT licence](crates/wasm-skill-sdk/LICENSE), so skills you write
carry no ELv2 obligations.

## Contributions

Contributions are accepted under the [Contributor Licence Agreement](CLA.md);
see [CONTRIBUTING.md](CONTRIBUTING.md).
