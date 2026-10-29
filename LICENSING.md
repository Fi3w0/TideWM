# Licensing

The repository contains software and separately licensed artwork. A license
for one component does not replace another component's terms.

| Component | License | Terms and notices |
| --- | --- | --- |
| TideWM source code, shaders, scripts, documentation, configuration examples and other project assets unless separately identified below | GNU GPL version 3 or later (`GPL-3.0-or-later`) | [LICENSE](LICENSE) |
| Bundled Wave implementation and `wavefmt` | GNU GPL version 3 or later (`GPL-3.0-or-later`) | [LICENSE](LICENSE); these are part of TideWM |
| `share/icons/tidewm.png` (the icon packages install) | Creative Commons Attribution-ShareAlike 4.0 International (`CC-BY-SA-4.0`) | [Logo notice](share/icons/LICENSE), [full legal text](LICENSES/CC-BY-SA-4.0.txt) |
| `share/icons/TideWM-logo-faithful-4k.png` | Creative Commons Attribution-NonCommercial-ShareAlike 4.0 International (`CC-BY-NC-SA-4.0`) | [Logo notice](share/icons/LICENSE), [full legal text](LICENSES/CC-BY-NC-SA-4.0.txt) |
| `protocols/hyprland-toplevel-export-v1.xml` (protocol definition from hyprwm/hyprland-protocols) | BSD 3-Clause (`BSD-3-Clause`) | [Copyright notice and license](protocols/LICENSE-hyprland-protocols) |
| `assets/fonts/AdwaitaSans-Regular.ttf` | SIL Open Font License 1.1 (`OFL-1.1`) | [Font copyright notices and license](assets/fonts/OFL-LICENSE.txt) |

Third-party dependencies retain their own copyright notices and licenses.
These project notices do not relicense third-party material.

## Logo attribution and use

TideWM has two renderings of its logo under different licenses:

- `share/icons/tidewm.png` is free artwork under CC BY-SA 4.0. It is the icon
  every package installs, so distributing TideWM, commercially or not, needs
  nothing beyond the GPL and this attribution.
- `share/icons/TideWM-logo-faithful-4k.png` stays under CC BY-NC-SA 4.0. It is
  kept in the repository but not installed by any package.

When sharing either file, credit Fi3w0, provide the license link and identify
modifications. A suitable attribution is:

> TideWM logo by Fi3w0 (2026), licensed under CC BY-SA 4.0.
> https://creativecommons.org/licenses/by-sa/4.0/
> Source: https://github.com/Fi3w0/TideWM/tree/master/share/icons

(use `CC BY-NC-SA 4.0` and https://creativecommons.org/licenses/by-nc-sa/4.0/
for the faithful 4k file). Shared adaptations must follow the ShareAlike
terms of the file they start from. Neither license grants trademark rights
or permission to imply official endorsement. See the full legal texts for
the binding conditions and exceptions.

Using the faithful 4k file beyond its NonCommercial condition needs separate
permission from [Fi3w0](https://github.com/Fi3w0). Earlier license grants
are not revoked by this notice.

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

The installers and distro packages install the CC BY-SA icon and include
the GPL text, logo attribution notice, CC BY-SA legal text and the font's OFL
notices in their `share/licenses` directory. Package metadata lists the
separate licenses, all of them free, so no unfree or license opt-in is
needed on Nix, Gentoo or elsewhere.
