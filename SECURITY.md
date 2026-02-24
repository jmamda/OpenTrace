# Security Policy

## Supported Versions

The following versions of OpenTrace are currently being supported with security updates:

| Version | Supported          |
| ------- | ------------------ |
| 0.3.x   | :white_check_mark: |
| < 0.3.0 | :x:                |

## Reporting a Vulnerability

If you discover a security vulnerability in OpenTrace, please report it responsibly.

**Please do NOT open a public GitHub issue for security vulnerabilities.**

Instead, report vulnerabilities by opening a [GitHub Security Advisory](https://github.com/jmamda/OpenTrace/security/advisories/new) in this repository.

### What to Include

- A description of the vulnerability and its potential impact
- Steps to reproduce the issue
- Any relevant logs, screenshots, or proof-of-concept code
- Your suggested fix (optional)

### What to Expect

- **Acknowledgement**: We will acknowledge receipt of your report within 48 hours.
- **Assessment**: We will assess the severity and validity of the report within 7 days.
- **Resolution**: We aim to release a fix within 30 days for critical vulnerabilities.
- **Credit**: With your permission, we will credit you in the release notes.

### Scope

OpenTrace runs entirely locally — no data leaves your machine. However, vulnerabilities that could affect:
- Local file system access or data exfiltration
- The proxy intercepting or tampering with LLM requests
- The SQLite database integrity
- The web dashboard exposing sensitive data

...are all within scope and taken seriously.
