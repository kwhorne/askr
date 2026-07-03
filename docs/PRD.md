# PRD — Elyra Askr

**En share-nothing, thread-per-core PHP-webserver i Rust**

| | |
|---|---|
| Status | Utkast v0.1 |
| Eier | Knut W. (Wirelabs AS) |
| Økosystem | elyra-conductor / Elyra |
| Kodenavn | `askr` (server) · bygger på `grove` (dev-verktøy) |
| Målstack | Laravel 13, Livewire/Volt, Flux UI v2, TailwindCSS v4 (TALL + VILT) |
| Sist endret | 2026-07-03 |

---

## 1. Sammendrag

Askr er en PHP-applikasjonsserver skrevet i Rust, bygget på tre idéer ingen har kombinert for PHP før: **share-nothing thread-per-core**, **copy-on-write-fork fra en varm master**, og **minnetrygghet som produkt**. Målet er å være den raskeste og mest effektive måten å serve Laravel på — i både dev og prod, fra samme binær.

Vi bygger *ikke* en ny reverse proxy. Den plassen er tatt (Pingora serverer 40M+ req/s i Rust allerede). Vi bygger app-server-laget — der feltet faktisk er umodent i Rust — og lar Askr fungere som en komplett enkeltbinær: TLS, statiske filer, cache og PHP i ett.

Den strategiske vinkelen er gratis: freenginx/F5-splitten i 2024 handlet bokstavelig talt om en CVE i minneutrygg C-kode (HTTP/3-modulen). «En webserver der hele hot-path er minnetrygg» er ikke marketing — det er lærdommen fra hele det dramaet.

---

## 2. Bakgrunn og problem

### 2.1 Hvorfor nå

Tre ting skjer samtidig:

- **nginx sin fremtid er usikker.** Etter F5-oppkjøpet forket kjerneutviklere ut (freenginx, Angie), governance er omstridt, og utløseren var en uenighet om sikkerhetshåndtering i C-kode. Feltet er i bevegelse for første gang på et tiår.
- **PHP-serving er modent nok til å utfordres.** FrankenPHP viste at man kan embedde PHP-tolken i en langtlevende prosess og droppe FastCGI-hoppene helt. Men FrankenPHP er bygget på Caddy (Go) og arver Go sin garbage collector.
- **Rust-verktøyene finnes nå.** io_uring-runtimes (monoio, glommio), quinn (QUIC), rustls, og modne FFI-mønstre gjør det mulig å bygge dette uten å skrive alt fra bunn.

### 2.2 Hvor de fire ekte skattene ligger

Flaskehalsen i PHP-serving er ikke HTTP-gjennomstrømning — Rust knuser det uansett. De reelle kostnadene er:

1. **Bootstrap per request** — autoloading, service providers, container-booting.
2. **IPC-hopp** — nginx → FastCGI → PHP-FPM → tilbake.
3. **GC-pauser** — gjelder Go-baserte løsninger som FrankenPHP.
4. **Isolasjon som koster** — enten betaler du i minne (prefork) eller i state-bleed (worker-modus).

Askr driver alle fire mot null *samtidig*. Det er hele tesen.

### 2.3 Landskapet i dag

| Løsning | Språk | Modell | Svakhet Askr angriper |
|---|---|---|---|
| PHP-FPM | C | Prosess per request via FastCGI | IPC-hopp, bootstrap per request |
| FrankenPHP | Go | Embedded PHP i Caddy, worker-modus | Go GC, state-bleed i worker-modus |
| RoadRunner | Go | PHP app-server, workers | Go GC, worker-pool warmup |
| Pingora | Rust | Proxy-rammeverk (ingen PHP) | Serverer ikke PHP i det hele tatt |
| Rymfony / pasir | Rust | Tynn FastCGI-proxy foran FPM | Ekte embedding mangler; umodent |

Konklusjon: ingen kjører embedded PHP i Rust med thread-per-core og CoW-fork. Det er den ledige nisjen.

---

## 3. Visjon, mål og ikke-mål

### 3.1 Visjon

Én Rust-binær som er det åpenbare valget for å kjøre Laravel — raskere enn FrankenPHP på benchmarks, tryggere enn nginx på arkitektur, og med ekte dev/prod-paritet.

### 3.2 Mål (v1)

- Embedde PHP in-process via SAPI (ingen FastCGI).
- Share-nothing thread-per-core på io_uring.
- CoW-fork fra en varm, ferdig-bootet Laravel-master → null bootstrap per request.
- Native HTTP/2 + HTTP/3, WebSocket, SSE.
- Enkeltbinær med innebygd statisk-server, tiered cache og TLS.
- Dev-modus (`grove serve`) og prod-modus (`askr serve`) fra samme kjerne.
- Førsteklasses støtte for TALL + VILT: Vite-HMR-proxy, Livewire/Volt, Reverb.

### 3.3 Ikke-mål (bevisst utenfor scope)

- **Generell reverse proxy / load balancer.** Bruk Pingora/Angie/HAProxy foran hvis du trenger det. Askr er en app-server, ikke en edge-proxy.
- **Polyglot.** Vi serverer PHP. Ikke Python, ikke Node. Fokus er en feature.
- **Windows som førsteklasses prod-mål.** io_uring er Linux. Dev på macOS/Windows kan falle tilbake på en enklere modell; prod er Linux.
- **Å konkurrere med FPM på «kjør hva som helst av gammel PHP».** Vi optimaliserer for moderne Laravel, ikke for hvert eksotiske extension.

---

## 4. Produktnavn og posisjonering i Elyra

To navn, ett tre:

- **`grove`** — det eksisterende dev-verktøyet (Valet/Herd-erstatter). Lunden der trær vokser: utviklerens lokale hage.
- **`askr`** — server-motoren. Asken — treet gudene formet det første mennesket av; det som ble *levende* og vender mot verden.

I praksis er `grove serve` bare Askr startet i dev-profil. Samme motor, to ansikter: lunden gror, asken lever. Dette gir den ekte dev/prod-pariteten som er halve salgsargumentet.

---

## 5. Arkitektur

### 5.1 Kjernemodellen: share-nothing, thread-per-core

Tenk ScyllaDB/seastar-filosofi, ikke nginx-filosofi. I stedet for én delt event-loop med låser, får hver CPU-kjerne sin egen verden: sin egen io_uring-ring, sin egen PHP-tolk, sin egen skive av connections. Null låser på hot-path, null cross-core-kontensjon.

```
        Warm master (Laravel booted once)
                     │
                 CoW fork
                     │
   ┌─────────────── One CPU core (×N) ───────────────┐
   │                                                  │
   │   io_uring ring   ──request──▶   PHP interpreter │
   │   accept·read·write             warm heap·arena  │
   │        ▲   │                                     │
   └────────│───│─────────────────────────────────────┘
     request│   │response
          (client edge)
```

io_uring eier hele klientkanten (både lese- og skrivepath). PHP-tolken er ren intern beregning — den rører aldri sockets. Skalering er trivielt: samme boks ×N kjerner, pinnet.

### 5.2 Request-livssyklus

1. **Boot én gang:** master-prosessen booter Laravel-kjernen frem til rett før routing (autoload, providers, container ferdig), og fryser tilstanden.
2. **CoW-fork:** per-core-workere forkes fra masteren. De arver den varme heapen gratis via copy-on-write-sider — ingen re-bootstrapping.
3. **Håndter request:** worker får en request, kjører i sin egen Zend-arena.
4. **Reset:** ved request-slutt enten (a) arena-reset + state-hooks (rask vei, betrodd trafikk) eller (b) worker dør og resirkuleres via ny fork (maks isolasjon).
5. **Respons:** io_uring skriver svaret tilbake.

Dialen mellom (a) og (b) settes per rute — se § 6.4.

### 5.3 PHP-embedding

Vi bruker PHPs embed-SAPI via FFI fra Rust. Tolken kjører in-process; ingen FastCGI, ingen egen FPM-pool. Zend-minnehåndtereren gir oss arena-semantikk vi kan utnytte for rask per-request-reset.

Kritisk detalj: vi kjører **non-ZTS** libphp med én tolk per prosess, og lar OS-en gi oss minnedelingen via CoW. Dermed slipper vi både ZTS-avgiften *og* re-bootstrappen. (Begrunnelse i § 6.1.)

### 5.4 I/O-lag

io_uring for hele I/O-pathen — accept, read, write — batchet og zero-copy der mulig. Dette er den konkrete effektivitetskanten mot nginx (epoll) og FrankenPHP (Go netpoller). Runtime: monoio eller glommio (§ 6.2).

### 5.5 Tiered enkeltbinær

Rust håndterer det som ikke trenger PHP: TLS-terminering, statiske filer, rate limiting, en enkel WAF, og respons-cache. PHP invokeres *bare* på dynamiske ruter. I praksis Varnish + nginx + FPM smeltet til én statisk-lenket binær. For selvhostet Outlet-infra i eget datasenter er det å slippe sidecar-jungelen en reell driftsgevinst.

### 5.6 Observability

Fordi vi eier både I/O-laget og PHP-embeddingen, kan vi korrelere en request ende-til-ende — HTTP-nivå + Zend-nivå — uten APM-agent. OpenTelemetry-native, per-request-flamegrafer på tvers av begge lag. Verken nginx eller FrankenPHP gir dette på det dypet.

---

## 6. Tekniske nøkkelbeslutninger

### 6.1 ZTS vs. non-ZTS

| | Non-ZTS + CoW-fork (valgt) | ZTS + tråder |
|---|---|---|
| Minnedeling | Gratis via OS copy-on-write | Delt opcache i prosess |
| Overhead | Ingen TSRM-avgift | ~10–15 % TSRM historisk |
| Kompleksitet | Fork-håndtering | Trådsikkerhet-helvete |
| Bootstrap | Null (arvet varm heap) | Må håndteres eksplisitt |

**Beslutning:** non-ZTS med CoW-fork. Vi får minnedelingen fra kjernen, ikke fra Zend.

### 6.2 Runtime: monoio vs. glommio vs. tokio

| | Modell | io_uring | Merknad |
|---|---|---|---|
| monoio | thread-per-core | io_uring-first | Aktiv, ByteDance, sannsynlig førstevalg |
| glommio | share-nothing | ja | Moden share-nothing, litt tregere puls |
| tokio | work-stealing | via kompat | Cross-thread-kontensjon — feil modell for oss |

**Beslutning:** monoio som primær, glommio som fallback-vurdering. tokio er utelukket for kjernen (men greit for sideverktøy).

### 6.3 Bygge fra bunn vs. på Pingora

Pingora gir et battle-tested proxy-lag. Men Askr er en *app-server*, ikke en proxy — vi trenger thread-per-core + embedded PHP, ikke en proxy-abstraksjon. Vi låner idéer og enkeltcrates (rustls, quinn) men bygger kjernen selv. Pingora-avhengighet ville dra inn en modell vi ikke vil ha.

### 6.4 State-reset-strategi

Dette er hele ballspillet. CoW-fork per request gir gratis isolasjon, men fork koster litt (syscall, page tables). Resirkulering av en worker over N requests bringer tilbake state-bleed i Laravel: statiske properties, container-singletons, superglobals.

Løsning: en **Octane-aktig integrasjonspakke** (`askr-laravel`) som nullstiller container og request-scope mellom kall, kombinert med Zend arena-reset. Rute-nivå-dial:

- `N = 1` → fork per request, maks isolasjon (utrygg/multi-tenant).
- `N = stor` → langtlevende worker, maks fart (betrodd egen app, f.eks. Inside NEXT POS).

Denne dialen er det som gjør Askr annerledes enn «FrankenPHP i Rust».

### 6.5 Extension-kompatibilitet

Den skjulte minen. Extensions med persistente ressurser (pconnect, enkelte PDO-drivere) oppfører seg stygt over fork. Vi trenger en testet whitelist og en fallback til fork-per-request-modus for extensions som ikke tåler CoW. FrankenPHP har brukt år på denne matrisen — det er delen vi ikke får gratis, og den må planlegges inn fra M1.

---

## 7. Build vs. bidra-til-FrankenPHP (ærlig vurdering)

Det ansvarlige spørsmålet: hvorfor ikke bare bidra til FrankenPHP?

**Argumenter for å bidra i stedet:**
- FrankenPHP har allerede løst det harde: embedding, worker-modus, extension-matrisen.
- Fellesskap og momentum finnes.
- Din tid er begrenset (GETS, Inside NEXT, flere sideprosjekter).

**Argumenter for å bygge Askr:**
- Go GC forsvinner ikke ved å bidra — den er arkitektonisk. Vår kjernedifferensiator (ingen GC, thread-per-core, io_uring) *krever* Rust.
- CoW-fork-per-request-modellen er en annen isolasjonsmodell enn FrankenPHPs worker-modus, ikke en inkrementell forbedring.
- Passer inn i et Rust-økosystem du allerede bygger (elyra-conductor, grove).
- «Minnetrygg webserver» som posisjonering krever at hele hot-path er Rust.

**Beslutning:** bygg — men stjel skamløst idéer og lærdom fra FrankenPHP, og vurder å bidra tilbake på felles interesser (f.eks. extension-kompatibilitetsdata). Start smalt (dev-server) for å validere embedding-antakelsene før du forplikter deg til prod-ambisjonen.

---

## 8. CLI og kommandostruktur

```
grove serve                 # dev-profil av Askr: HMR, hot reload, intern CA
grove serve --php 8.4       # velg PHP-versjon
grove link                  # registrer lokalt Laravel-prosjekt
grove secure <site>         # TLS via intern CA

askr serve                # prod-profil
askr serve --workers auto # thread-per-core, auto = antall kjerner
askr serve --isolation strict   # tvinger fork-per-request globalt
askr bench <url>          # innebygd benchmark mot FPM/FrankenPHP
askr doctor               # sjekk extensions, ZTS-build, io_uring-støtte
askr config check         # valider typet config, dry-run
```

Config er typet og hot-reloadbar (KDL eller Rhai-basert), med *convention over configuration*: auto-detekter Laravel `public/`, auto-wire routing. Ingen nginx-DSL å bale med.

---

## 9. Dev-server-krav (TALL + VILT)

Siden dette skal kjøre din faktiske stack, må dev-modus være kompromissløst god på:

- **Vite-HMR-proxy** native i dev — proxier Vite dev-serveren for TailwindCSS v4 og Flux UI v2-assets uten manuell konfig.
- **Livewire/Volt** — ren WebSocket/long-poll-håndtering, ingen buffering-feller.
- **Reverb** — Laravels WebSocket-server proxiet uten sidecar.
- **Intern CA** — HTTPS lokalt out of the box (`grove secure`).
- **Hot reload** av PHP-kode uten å drepe den varme masteren (invalidér opcache selektivt).
- **Auto-detect** av `.env`, `public/`, PHP-versjon per prosjekt.
- **Feilsider** med Zend-nivå-stacktrace + HTTP-kontekst i samme visning.

Mål: `git clone && grove serve` og du er oppe på et fungerende Laravel 13 + Flux v2 + Tailwind v4-oppsett med HTTPS og HMR på under ett minutt.

---

## 10. Prod-server-krav

- Graceful reload uten downtime (Sozu viser at hot config-reload er mulig).
- HTTP/3 + QUIC default, HTTP/2 fallback.
- Innebygd respons-cache og statisk-server (tiered, § 5.5).
- Rate limiting og enkel WAF på Rust-siden.
- OTel-eksport, Prometheus-metrics, per-request-tracing.
- `askr doctor` som pre-flight før deploy: verifiser extension-whitelist, ZTS/non-ZTS-build, io_uring-kjernestøtte.
- Deterministisk minnebudsjett per worker (unngå fork-bomber under last).

---

## 11. «Future of nginx» / sikkerhetsposisjonering

Kjernebudskapet, klart formulert: **hele hot-path er minnetrygg Rust. PHP er den eneste `unsafe`-grensen, og den sandboxes** (seccomp, Landlock på Linux). freenginx-splitten kom av en CVE i minneutrygg C-kode i HTTP/3-modulen — Askrs arkitektur gjør den klassen av feil strukturelt umulig i serverlaget. Dette er ikke en fotnote; det er overskriften i enhver blogpost og README.

---

## 12. Roadmap

| Fase | Mål | Innhold | Grov innsats |
|---|---|---|---|
| **M0 — Spike** | Bevis at embedding funker | libphp embed-SAPI via FFI fra Rust, «hello world» fra PHP i-prosess, non-ZTS-build | 2–3 uker |
| **M1 — Dev-server** | `grove serve` kjører ekte Laravel 13 | io_uring-loop, embedded PHP, statiske filer, intern CA, Vite-HMR-proxy, Livewire/Volt | 2–3 mnd |
| **M2 — Warm master + CoW** | Null bootstrap per request | Warm master, CoW-fork, arena-reset, `askr-laravel` state-hooks, isolation-dial | 2–3 mnd |
| **M3 — Prod-herding** | Slå FrankenPHP på benchmark + trygg | Extension-matrise, HTTP/3, graceful reload, WAF, OTel, seccomp/Landlock, `askr doctor` | 4–6 mnd |
| **M4 — Polish** | Utgivelsesklar | Docs, `askr bench`, pakker for Debian/nix, blogpost-lansering | løpende |

Realistisk: fungerende dev-server på et par måneders fokusert arbeid. Prod-server som slår FrankenPHP *og* er trygg på extensions er et år+ og et lite team. Vær ærlig om det fra dag én.

---

## 13. Suksesskriterier

- **Ytelse:** høyere req/s og lavere p99-latens enn FrankenPHP worker-modus på en referanse-Laravel-app, målt med `askr bench`.
- **Effektivitet:** lavere minne per samtidig request enn FPM ved samme throughput.
- **Paritet:** samme binær kjører identisk oppsett i dev og prod.
- **Trygghet:** null minneutrygg kode i serverlaget (verifiserbart: `#![forbid(unsafe_code)]` utenom FFI-grensen).
- **DX:** `git clone && grove serve` → oppe på under ett minutt på et TALL+VILT-prosjekt.

---

## 14. Risiko og åpne spørsmål

| Risiko | Alvor | Demping |
|---|---|---|
| Rust embed-SAPI-FFI umoden vs. Go-SDK | Høy | M0-spike før alt annet; fall tilbake på FastCGI-bro hvis embedding blokkerer |
| Extension-fork-inkompatibilitet | Høy | Whitelist + fork-per-request-fallback; lån FrankenPHPs data |
| State-bleed i Laravel worker-modus | Middels | `askr-laravel`-pakke, arena-reset, isolation-dial |
| Tidsbudsjett (soloprosjekt) | Middels | Start på dev-server; prod er separat go/no-go etter M2 |
| io_uring kjernekrav i prod | Lav | Doctor-sjekk; dokumentér minimum kjerneversjon |

Åpne spørsmål:
- KDL vs. Rhai for typet config?
- Skal M1 dev-server bruke CoW allerede, eller enkel prefork først?
- Lisens — MIT/Apache-2.0 (Wirelabs-standard) vs. noe med sterkere copyleft?
- Hvor mye av `askr-laravel` kan gjenbruke Octanes eksisterende reset-logikk?

---

## 15. Referanser

- FrankenPHP — embedded PHP i Caddy, worker-modus (stabil 1.x, mai 2026).
- Cloudflare Pingora — Rust proxy-rammeverk, 40M+ req/s.
- freenginx / Angie — nginx-forkene etter F5-splitten (2024), utløst av CVE-håndtering i HTTP/3-kode.
- monoio / glommio — thread-per-core io_uring-runtimes i Rust.
- Laravel Octane — referansemodell for state-reset mellom requests.
- Wizer / CRIU — snapshot/pre-initialisering (fremtidig utforskning for WASM-sporet).
