Devin CLI 3000.12.57 on macOS, recorded 2026-11-14 in a 40x120 pty. Secret values (state,
code_challenge) and personal values are replaced. The .bin files are the exact bytes it printed.

1-picker.bin      `devin auth login` (no flag) up to its first prompt, which waits for a key.
                  The only queries in it are OSC 10/11 colour queries (ESC]10;? BEL, ESC]11;? BEL).
                  It also pushes kitty keyboard flags (ESC[>1u); that is not a query.
                  It holds no DA1, kitty query (ESC[?u) or CPR. The stand-in devin in
                  service/accounts_tests.rs asks those as well, for robustness only.
2-browser.bin     After Enter on option 1: "Waiting for login in browser...", then
                  "Verifying account...", then "Login successful! Credentials stored.", with OSC 10/11
                  queries along the way. Devin opens the page itself; no address is printed.
3-manual.bin      `devin auth login --force-manual-token-flow`: no picker. "Visit <URL> to sign in...",
                  with the URL wrapped in an OSC 8 hyperlink, then the " Code:" prompt.
4-status-in.txt   `devin auth status` signed in.
5-status-out.txt  `devin auth status` signed out.
