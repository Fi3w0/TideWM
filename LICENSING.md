# Licensing

The repository contains software and separately licensed artwork. A license
for one component does not replace another component's terms.

| Component | License | Terms and notices |
| --- | --- | --- |
| TideWM source code, shaders, scripts, documentation, configuration examples and other project assets unless separately identified below | GNU GPL version 3 or later (`GPL-3.0-or-later`) | [LICENSE](LICENSE) |
| Bundled Wave implementation and `wavefmt` | GNU GPL version 3 or later (`GPL-3.0-or-later`) | [LICENSE](LICENSE); these are part of TideWM |
| `share/icons/TideWM-logo-faithful-4k.png` and `share/icons/tidewm.png` | Creative Commons Attribution-NonCommercial-ShareAlike 4.0 International (`CC-BY-NC-SA-4.0`) | [Logo notice](share/icons/LICENSE), [full legal text](LICENSES/CC-BY-NC-SA-4.0.txt) |
| `assets/fonts/AdwaitaSans-Regular.ttf` | SIL Open Font License 1.1 (`OFL-1.1`) | [Font copyright notices and license](assets/fonts/OFL-LICENSE.txt) |

Third-party dependencies retain their own copyright notices and licenses.
These project notices do not relicense third-party material.

## Logo attribution and use

When sharing the logo, credit Fi3w0, provide the license link and identify
modifications. A suitable attribution is:

> TideWM logo by Fi3w0 (2026), licensed under CC BY-NC-SA 4.0.
> https://creativecommons.org/licenses/by-nc-sa/4.0/
> Source: https://github.com/Fi3w0/TideWM/tree/master/share/icons

The logo license permits noncommercial sharing and adaptation. Shared
adaptations must follow its ShareAlike terms. It does not grant trademark
rights or permission to imply official endorsement. See the full legal text
for the binding conditions and exceptions.

TideWM's GPL software remains usable and distributable commercially under
the GPL. A distributor relying on the logo's CC license must separately
respect its NonCommercial condition; commercial distributions can omit or
replace the standalone icon, or obtain separate permission from
[Fi3w0](https://github.com/Fi3w0). Earlier license grants are not revoked by
this notice.

## Wave and standalone Scorium

Wave is TideWM's configuration format. This checkout builds its embedded
implementation from `src/tide_core/wave.rs`, `waves.rs` and `wave_fmt.rs`,
with the `wavefmt` tool in `src/bin/wavefmt.rs`. That bundled implementation
is GPL-licensed with TideWM; this checkout does not depend on the standalone
Scorium library.

[Standalone Scorium](https://github.com/Scorium-lang/scorium-rs) is a
separately licensed project. Its [actual license
file](https://github.com/Scorium-lang/scorium-rs/blob/main/LICENSE) is headed
"PolyForm Strict License 1.0.0" and contains custom source-available terms.
Read that file and its [licensing
guide](https://github.com/Scorium-lang/scorium-rs/blob/main/docs/LICENSING.md)
for permissions concerning personal use, commercial use, redistribution and
contribution-focused forks. Do not assume its custom terms are identical to
an unmodified standard PolyForm license.

Scorium's terms govern the separately published Scorium implementation;
linking to them here does not impose its restrictions on TideWM or on
configuration files users write themselves. A future integration of the
standalone library must establish suitable licensing before shipping.

## Packaged notices

The installers and distro packages include the GPL text, logo attribution
notice, CC BY-NC-SA legal text and the font's OFL notices in their
`share/licenses` directory. Package metadata lists the separate licenses.
Because the bundled logo is noncommercial, the Nix package is classified as
unfree; see [packaging/README.md](packaging/README.md) for a package-specific
opt-in. Gentoo users may need to accept `CC-BY-NC-SA-4.0` for this package.
