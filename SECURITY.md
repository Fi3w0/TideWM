# Security Policy

## Reporting a Vulnerability

Please don't open a public issue for security vulnerabilities. Instead:

- Use GitHub's private vulnerability reporting (Security tab → "Report a vulnerability") on this repo, or
- Contact the maintainer ([@Fi3w0](https://github.com/Fi3w0)) directly.

Include what you found, how to reproduce it, and its potential impact.

## Response

- Acknowledgement within 1 week.
- Fix timeline depends on severity. Critical issues (privilege escalation, memory corruption reachable by a compromised client, etc.) are prioritized; low-severity issues are folded into normal release cycles, generally within 30 days.

## Scope

TideWM is a Wayland compositor, so it mediates untrusted client input by design. Reports involving crashes, memory unsafety, or privilege issues triggerable by a malicious Wayland client are especially welcome.
