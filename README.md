# AI tým

Autonomní AI vývojový tým nad tvými projekty, postavený na [Claude Code](https://claude.com/claude-code) (`claude -p`) a ElevenLabs.

- **Šéf (PM)** pravidelně projde každý projekt (docs, `PROGRESS.md`, git log, otevřené PR) a naplánuje úkoly podle deadlinů.
- **Developeři** makají paralelně v izolovaných git worktree (můžou si spouštět vlastní subagenty), commitují, a orchestrátor otevře PR.
- **Meeting room** (hlas, česky): ElevenLabs STT → tým → ElevenLabs TTS, každá role má svůj hlas. Hlasem jde přidávat úkoly, měnit deadliny a priority, pozastavit projekt a schválit merge nebo deploy.
- **„Zavolání“**: když tým potřebuje tebe (rozhodnutí, ruční test), vytvoří událost v Google Kalendáři a pošle notifikaci (desktop, volitelně ntfy).
- **Šetření usage**: denní strop běhů, pauza podle aktuálního 5h bloku (`ccusage`), backoff při vyčerpaném limitu, quiet hours.

Bezpečnost: API vyžaduje token vygenerovaný při každém startu (ochrana proti CSRF) a kontroluje hlavičku Host (ochrana proti DNS rebinding). Server poslouchá jen na `127.0.0.1`. Merge a deploy tým jen navrhne. Provedou se, až potvrdíš přesně zobrazený cíl (tlačítkem nebo „schvaluju“), jedno schválení platí pro jednu akci a jen pro PR, které tým otevřel. Dev agenti nemají funkční přihlášení k GitHubu ani git push (prázdný credential helper, žádný SSH agent) a git hooky orchestrátor nespouští. **Limit:** CLI nástroje pro deploy s vlastním uloženým přihlášením (firebase, vercel…) jsou pro agenta dostupné přes shell. Pro tvrdou izolaci spouštěj agenty pod jiným uživatelem nebo v kontejneru.

## Požadavky
Rust (stable), [Claude Code](https://claude.com/claude-code) přihlášený k předplatnému, `gh` (přihlášený), `git`, ElevenLabs API klíč. Volitelně `ccusage` (sledování usage), `notify-send`.

## Instalace
```bash
git clone https://github.com/cufelix/ai-team && cd ai-team
cp team.example.toml team.toml      # role, projekty, rozvrh, limity
cp .env.example .env && chmod 600 .env   # doplň ELEVENLABS_API_KEY
cargo build --release
./target/release/ai-team serve --idle    # jen meeting room na http://localhost:7777
./target/release/ai-team serve           # meeting + autonomní práce
```
Jako služba: `systemd-ai-team.service` zkopíruj do `~/.config/systemd/user/ai-team.service` a spusť `systemctl --user enable --now ai-team`.

## CLI
`serve [--idle]` · `tick` (jedno kolo práce) · `plan <projekt>` · `status` · `auth-google` · `call "<text>"`

## Google Kalendář (volitelné)
V Google Cloud Console vytvoř OAuth client typu *Desktop app* a ulož JSON jako `state/google_client.json`. Pak spusť `ai-team auth-google`. Potom se samy zakládají povinné standupy (`[standup]` v `team.toml`) a ad-hoc „hovory“. Bez připojení dostaneš v notifikaci odkaz „přidat do kalendáře“.

## Struktura
```
team.example.toml  šablona konfigurace (team.toml je v .gitignore)
src/orchestrator.rs  plánování, dev běhy ve worktree, PR, usage gate, standupy
src/meeting.rs       HTTP API + meeting logika, akce schvalované hlasem
src/claude.rs        headless Claude Code runner + povolené/zakázané nástroje
src/voice.rs         ElevenLabs STT/TTS
src/gcal.rs          Google Calendar OAuth + události
src/board.rs         stav (úkoly, log) → state/board.json
web/index.html       meeting room
```

## Licence
MIT
