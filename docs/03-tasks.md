# Breakdown de tarefas — EmbedMind

> Documento canônico do pacote SDD (03 de 04). Fonte de verdade do **em que ordem**.
> Atualizado em 08/jul/2026 contra o estado real do repo (13 commits, M1 ~85% entregue).
> Duas marcações governam este documento:
>
> - **[✅ ENTREGUE]** — código existe em `main`, com testes. Agentes NÃO devem refazer;
>   apenas manter `cargo test --workspace` verde ao mexer perto.
> - **[MANUAL — founder]** — tarefa humana (posts, launch, decisões comerciais).
>   Agentes NÃO executam; no máximo preparam insumos quando indicado.
>
> Linha do tempo: launch público dia 35 ≈ **11/ago/2026** (hard stop) · go/no-go dia 90
> ≈ **05/out/2026**. Ritmo: release a cada 2–3 semanas; todo marco termina em algo
> público.

## Estado atual — já entregue, não refazer

| Item | Entrega | Evidência |
|---|---|---|
| 1.1 [✅ ENTREGUE] | Formato de arquivo único + WAL/crash-safety (header, pager, WAL, recovery, Vfs + FaultVfs) | `crates/embedmind-core/src/{format,storage}` + `tests/crash.rs` |
| 1.2 [✅ ENTREGUE] | KV store + API Rust (`record`, B-tree, `api::Store`, overflow chains, timeline) | `tests/crash_records.rs`, CHANGELOG |
| 1.3 [✅ ENTREGUE] | HNSW paginado (endereçamento direto, ADR 0008) + embeddings ONNX embarcados + chunking de memórias longas | `embedmind-core::{index,embed,recall}` |
| 1.4 [✅ ENTREGUE] | Servidor MCP `remember`/`recall`/`forget` — stdio JSON-RPC direto (ADR 0009) | `crates/embedmind-mcp` + E2E via pipes |
| 1.5 [✅ ENTREGUE] | Memória automática de contexto de projeto (raiz git / `.embedmind.toml`) | `detect_project`, testes |
| 1.6 (parte) [✅ ENTREGUE] | CLI completo: `serve/remember/recall/forget/stats` + `vacuum` com erro explícito; `Store::stats` | `crates/embedmind-cli` + E2E |
| 1.8 [✅ ENTREGUE] | Crash-recovery no CI + fuzzing (5 alvos, corpus versionado, passe curto por PR + noturno) | `fuzz/`, `.github/workflows/ci.yml` |

Comando que prova o estado: `cargo test --workspace` verde nas 3 plataformas do CI.

---

## Fase A — fechar o M1 (v0.1 lançável) — 1–2 semanas

### A1. Pipeline de release: binários pré-compilados (resto do item 1.6) [✅ ENTREGUE]

Job de release no GitHub Actions (`.github/workflows/release.yml`) que, numa tag `v*`,
builda binários release (Windows/Linux/macOS, LTO+strip conforme `Cargo.toml`), roda a
suite como gate, anexa os artefatos ao GitHub Release e valida o teto de tamanho.

- **DoD:** tag de teste produz 3 artefatos baixáveis; artefato < 40 MB com modelo
  embutido; `embedmind --version` funciona no artefato baixado.
- **Verificação:** disparar o workflow numa tag `v0.1.0-rc1` e conferir artefatos +
  `gh release view`.
- **Nota de tamanho:** o binário nu dá ~45 MiB (ONNX Runtime estático via `ort`); o
  teto de 40 MB passou a governar o **artefato comprimido** (~20 MiB), decisão
  registrada em [ADR 0010](adr/0010-teto-de-tamanho-governa-artefato-comprimido.md) e
  refletida na spec S8. Disparar a tag e o `cargo publish` real seguem
  [MANUAL — founder].

### A2. Preparar publicação no crates.io (resto do item 1.6) [✅ ENTREGUE]

Metadados de publicação (`description`, `repository`, `homepage`, `keywords`,
`categories`, `readme`, `license = "MIT"`) nos 3 crates; deps internas pinadas
com `path` **e** `version` em `[workspace.dependencies]`; README por crate;
ordem de publicação e passos `[MANUAL — founder]` documentados em
[RELEASING.md](RELEASING.md).

- **DoD:** `cargo publish --dry-run -p embedmind-core` limpo; `cargo package
  --workspace` empacota + compila os 3 (o dry-run de mcp/cli só resolve após o
  core estar no índice — ordem obrigatória, ver RELEASING.md); nomes
  `embedmind` / `embedmind-core` / `embedmind-mcp` confirmados **disponíveis**.
- **Verificação:** `cargo package --workspace` verde; dry-run do core limpo;
  `cargo clippy`/`fmt` limpos. (O `cargo publish` real é [MANUAL — founder].)
- **Achado (bloqueia o publish real do core, `[MANUAL — founder]`):** o
  `embedmind-core` empacota em ~16 MiB comprimido (modelo ONNX embarcado, ADR
  0004), acima do teto de 10 MiB do crates.io. O `--dry-run` passa (teto é
  server-side), mas o `cargo publish` real exige **pedido de aumento de limite**
  ao crates.io antes de publicar o core — detalhado em RELEASING.md.

### A3. Harness de benchmark + números honestos (item 1.7, parte dev)

Implementar `benches/` conforme [BENCHMARKS.md](BENCHMARKS.md): datasets
`agent-mem-10k/-100k` (gerador committado), brute-force baseline, medição de
recall@10, p50/p99 (quente + cold-open), throughput de ingest, tamanho de arquivo, RSS;
comparação com sqlite-vec e zvec em versões pinadas; saída em tabela markdown pronta
para o README.

- **DoD:** `benches/run_all.sh` (ou `cargo bench`) roda fim a fim e emite a tabela;
  metodologia do doc respeitada (mesmos embeddings para todos, versões registradas);
  NFRs da spec (recall < 50 ms p99 @ 100k) medidos.
- **Verificação:** rodar o harness completo e revisar a tabela gerada.
- **Parte 1 — feita:** crate `embedmind-bench` no workspace com gerador de
  corpus determinístico (`corpus::generate`, seed registrada), specs committadas
  `agent-mem-10k/-100k`, materialização via ONNX embarcado (`.mind` + sidecar
  `.vec` com guard de seed/dims/modelo), baseline brute-force exato e medição de
  `recall@10` (HNSW vs baseline, por conjunto). Binários `gen_dataset`/`baseline`;
  smoke end-to-end em `benches/tests/harness.rs` no `cargo test --workspace`.
  Referência medida: `baseline agent-mem-10k` → recall@10 0.9945 (min 0.90).
- **Parte 2 — feita:** módulos `metrics` (p50/p99 nearest-rank + throughput),
  `sysmem` (RSS de pico via `sysinfo` pinado, sem `unsafe`), `harness::run_suite`
  (recall@10, p50/p99 quente, cold-open = `Store::open` + 1ª query com o store
  fechado antes por causa do single-writer, `remember` p50/p99 fim a fim,
  throughput de ingest, tamanho de arquivo, RSS de pico), `competitors` (registro
  sqlite-vec/zvec com versões **pinadas e registradas** + adaptadores gated por
  feature `compare-*`; quando a toolchain nativa falta, reporta honestamente
  "not measured on this run (target vX.Y)", nunca número inventado) e `report`
  (validação dos NFRs da spec, tabela markdown pronta para o README com seção
  "where EmbedMind loses", e JSON de resultados). Binário `run_all` + script
  `benches/run_all.sh` rodam fim a fim, gravam `results/<version>.json` +
  `latest.md`, e saem com código ≠ 0 se algum NFR aplicável falhar (serve de
  guard de regressão no CI, BENCHMARKS.md §5).
- **Números medidos (Windows dev box, 20 CPUs lógicas, CPU-only, single-thread):**
  `agent-mem-10k` → recall@10 0.9953 (min 0.90), query p50/p99 quente 10.6/17.1 ms,
  cold-open 0.3 ms + 1ª query 12 ms, `remember` p50/p99 6.7/16.7 ms, ingest ~82
  mem/s, arquivo 82 MiB, RSS de pico ~112 MiB. NFR `remember` p99 < 200 ms: ✅.
  Os NFRs enunciados @ 100k (recall p99 < 50 ms, RAM < 300 MB) exigem o
  `agent-mem-100k` — ver `docs/BENCHMARKS.md`/CHANGELOG para o resultado.

### A4. README final de launch (item 1.7, parte dev do conteúdo) [✅ ENTREGUE]

README atualizado para o estado v0.1 lançável: linha de status v0.1 no topo (aviso
pre-v0.1 removido), seção **Install** separada do **Quickstart** (binário pré-compilado
dos artefatos do `release.yml` — `embedmind-{linux-x86_64.tar.gz,macos-aarch64.tar.gz,
windows-x86_64.zip}` — mais `cargo install embedmind` e build por fonte); tabela de
benchmark **real** renderizada de `benches/results` (recall@10 0.9953, query p99 17.1 ms,
`remember` p99 16.7 ms, arquivo 82 MiB, RSS ~112 MiB) com caveats honestos (ingest
inclui embedding e não compara com vetor-só; sqlite-vec/zvec *not measured* nesta run,
sem número inventado; NFRs @100k pendentes do dataset 100k); seção **When to use
sqlite-vec instead**; claims de full-text/graph escopadas ao roadmap (nada não-entregue
prometido como v0.1). Roteiro do GIF de 30s committado em
[docs/launch/gif-script.md](launch/gif-script.md) (sequência exata de comandos +
timing; gravação é [MANUAL — founder]).

- **DoD:** README sem promessas não-entregues; benchmark com números reais e derrotas
  incluídas; quickstart validado literalmente (copy-paste funciona).
- **Verificação:** quickstart rodado fim a fim contra o binário release em arquivo
  `.mind` limpo — `remember` → `recall` (recall semântico: "why tokio?" acha a memória)
  → `stats` (modelo `all-MiniLM-L6-v2-int8`, contagens corretas). ✅
- Nota: o GIF de demo de 30s e o teste com um 2º agente além do Claude Code são
  [MANUAL — founder]; esta task entregou o roteiro do GIF (sequência de comandos a gravar).

### A5. [MANUAL — founder] Fechar o M1

Gravar o GIF, testar com 2º agente (Cursor ou outro), `cargo publish` real, criar o
GitHub Release v0.1.0. **Gate:** M1 completo = pré-condição do launch (dia 35).

---

## Fase B — M2: lançamento público + híbrido — semanas 5–8

### B1. [MANUAL — founder] Dia 35: repo público + lançamento coordenado (item 2.1)

Show HN, r/ClaudeAI, r/LocalLLaMA, r/rust, X. Post: *"I built persistent memory for
coding agents in Rust — single file, no server"*. Itens 2.2 (responder issues < 24h) e
2.7 (releases quinzenais guiados por issues) são processo contínuo do founder.

### B2. Full-text search na engine (item 2.3) [✅ ENTREGUE]

Índice invertido próprio nas páginas (default do DESIGN §12, decidido vs. tantivy em
[ADR 0011](adr/0011-full-text-indice-invertido-proprio.md)), integrado ao WAL/transações,
com BM25; fusão com o ranking vetorial por RRF k=60 (ADR 0005) no `recall`.

- **DoD:** stories S9 da spec verdes (termo exato raro encontra a memória; fusão nunca
  exige interseção; arquivo antigo degrada para vetor-só com aviso); crash tests
  cobrindo as páginas novas; fuzz target para o parser do índice se houver formato
  novo; ADR escrito.
- **Verificação:** `cargo test --workspace` + casos de ouro da S9. ✅
- **Onde:** engine half — `index::fts` (dicionário paginado + BM25 k1=1.2/b=0.75,
  `Store::search_text`), page types `FTS_DICT`/`FTS_POSTINGS`, `format_version` 1→2
  aditivo (FORMAT.md §11), fuzz target `fuzz_fts_page`. Recall half — `recall::fuse`
  (RRF k=60) fundindo HNSW + BM25 em `Store::recall`/`recall_detailed`;
  `Store::recall_vector` isola o HNSW puro para o benchmark harness.

### B3. Filtros de metadados no `recall` (item 2.4) [✅ ENTREGUE]

`recall(query, filters: {...})` na API, MCP (extensão do schema da tool) e CLI;
semântica AND; mesma garantia anti-sub-retorno do ef_search adaptativo.

- **DoD:** story S10 verde; filtros compostos com escopo de projeto e tombstones;
  schema MCP atualizado de forma retrocompatível.
- **Verificação:** `cargo test -p embedmind-core filters` + E2E MCP.

### B4. `embedmind vacuum` real (v0.2, promessa registrada no CLI) [✅ ENTREGUE]

Reconstrução por cópia (nunca in-place): novo arquivo sem tombstones/overflow órfãos,
índices reconstruídos, troca atômica no final; crash em qualquer ponto preserva o
original.

- **DoD:** story S11 verde; crash test da varredura cobrindo o vacuum; `stats` antes/
  depois mostra redução.
- **Verificação:** teste round-trip ingest → forget 50% → vacuum → invariantes.

### B5. Bindings Python (item 2.5) [✅ ENTREGUE]

Crate `bindings/python` (PyO3 + maturin, workspace próprio como `fuzz` — PyO3 gera
`unsafe`, que o lint `unsafe_code = "forbid"` do workspace principal rejeitaria).
Expõe `Store` com `remember`/`recall`/`forget`/`stats`/`vacuum` na mesma semântica das
tools MCP/CLI — casca fina sobre `embedmind_core::api`, sem lógica de domínio
(CLAUDE.md decisão 2). Metadados tipados ↔ escalares Python nativos; filtros de
`recall` aceitam escalar (Eq) ou tupla `(min, max)` (Range, S10); filtro por agente +
breakdown por agente em `stats` (S14) passam sem mudança. Wheel `abi3` (uma por
plataforma, CPython 3.9+) com modelo ONNX embutido; stubs `.pyi` + `py.typed`.

- **DoD:** `pip install` do wheel local funciona; suite pytest espelhando os E2E do
  CLI; mesmos arquivos `.mind` legíveis por Rust e Python. ✅ 20 testes verdes,
  incl. round-trip cruzado (escreve em Rust, lê em Python e vice-versa).
- **Verificação:** `maturin build && pip install ... && pytest`.
- **Onde:** `bindings/python/{src/lib.rs,tests/}`; CI: job `wheels` (3 plataformas) em
  `release.yml` + job `python-bindings` (lint + pytest) em `ci.yml`. Publicação no PyPI
  fica MANUAL (founder), como o crates.io.

### B6. [MANUAL — founder] 2º post técnico (item 2.6)

A engine por dentro (WAL, HNSW em arquivo único). Agente pode preparar rascunho
técnico com diagramas se solicitado — publicação é do founder.

---

## Fase BQ — Qualidade de busca em escala + benchmarks honestos (pré-launch, alta prioridade)

> Origem: análise dos resultados de 2026-07-09 (`benches/results/0.1.0-dev.json`).
> Dois achados: (1) recall@10 a 100k caiu para 0,9313 na média e **0,20 na pior
> query** — `ef_search` fixo em 64 não escala com o índice; (2) a tabela de
> comparação é assimétrica — o cronômetro do EmbedMind inclui embeddar a query e
> carregar registros completos, o dos concorrentes mede busca vetorial pura sobre
> vetor pronto. Ambos ferem a credibilidade dos números que o README vai publicar
> no launch — por isso esta fase precede o dia 35.

### BQ1. `ef_search` proporcional ao tamanho do índice (story S16) [⚠️ IMPLEMENTADO, DoD REPROVADO]

Substituir o default fixo (`HNSW_DEFAULT_EF_SEARCH = 64` em `format.rs`) por um
default que escala com o número de nós do índice — fórmula/patamares decididos por
sweep no harness (ex: candidatos `max(64, k·ln N)` e degraus por faixa; escolher pelo
dado, registrar em ADR). `Query::ef_search(n)` explícito segue soberano. Aproveitar a
folga: p99 medido 15,5 ms @ 100k vs. teto de 50 ms — há ~3x de orçamento de latência
para comprar recall.

- **DoD:** story S16 verde — recall@10 @ 100k ≥ 0,95 média e ≥ 0,70 pior query com
  p99 < 50 ms; sem regressão de latência no 10k além do limiar §5; ADR do
  escalonamento escrito; harness reporta distribuição do recall por query
  (mín/p10/p50).
- **Verificação:** `benches/run_all.sh` nos dois datasets + `cargo test --workspace`.
- **Atenção (interação com RAM):** a meta de RSS < 300 MiB @ 100k está com 6% de
  folga (280,9 medido) — validar RSS na mesma rodada; se estourar, reportar e decidir
  em ADR (não esconder).
- **Validação 2026-07-11 (`benches/run_all.sh --full`, 1000 queries, ver ADR
  0015):** o mecanismo (degraus + `Query::ef_search` soberano + harness
  reportando mín/p10/p50) está implementado, testado e correto — mas o DoD da
  story **reprova em três eixos**: recall@10 média @ 100k = 0,9360 (< 0,95),
  pior query = 0,20 (< 0,70), query p99 híbrido = 1224,62 ms (>> 50 ms, por um
  gargalo pré-existente do FTS, fora de escopo — postings decodificadas
  inteiras por query, custo linear no corpus). RSS de pico também estourou:
  307,1 MiB > 300 MiB (a folga de 6% citada acima já não existe a 100k).
  10k segue saudável, sem regressão. Detalhes e follow-ups no ADR 0015.

### BQ2. Latência decomposta + artefatos consistentes (story S17, metade EmbedMind)

Harness passa a medir e reportar separadamente o custo de embeddar a query
(`embed_ms`) e o custo do motor (busca híbrida + fusão + carga de registros) — o
`SuiteResult` já carrega `query_vectors`, parte do encanamento existe. Na mesma task:
`results/<versão>.json` e `latest.md` saem da mesma invocação (hoje divergem), e cada
linha da tabela declara o escopo do sistema (devolve ids vs. conteúdo; persiste só
vetores vs. texto+metadados+índices).

- **DoD:** tabela emite `query = embed X ms + engine Y ms`; md/json consistentes por
  construção; nota de escopo por sistema; teste do renderer cobrindo a decomposição.
- **Verificação:** `benches/run_all.sh` e revisão da tabela gerada.

### BQ3. Comparação texto→resultado simétrica (story S17, metade concorrentes)

Nova seção da tabela onde sqlite-vec e zvec pagam o mesmo pedágio: a query é
embeddada com o mesmo modelo/pipeline ONNX (fora deles, tempo medido e somado) e o
resultado é comparado fim a fim com o `recall` do EmbedMind. A seção index-only
existente continua — ela responde outra pergunta (qualidade do índice) e é onde o
zvec vence hoje legitimamente.

- **DoD:** tabela com as duas seções rotuladas (index-only e texto→resultado);
  metodologia atualizada em BENCHMARKS.md; regras de honestidade do §4 preservadas.
- **Verificação:** `benches/run_all.sh` com as features `compare-*` habilitadas.

### BQ4. Concorrente da categoria de produto: Chroma local (story S18) [✅ ENTREGUE]

Adaptador de comparação para o Chroma em modo local/embedded (versão pinada), medido
na seção texto→resultado com o mesmo all-MiniLM-L6-v2 — a alternativa real que um dev
de agente considera. Driver via subprocess/binding Python, gated por feature como os
demais; sem toolchain, reporta "not measured".

- **DoD:** linha do Chroma na tabela texto→resultado (recall@10, p50/p99, tamanho em
  disco) ou "not measured" honesto; versões pinadas registradas.
- **Verificação:** `benches/run_all.sh` num ambiente com Python + Chroma instalados.
- **Dependência externa (founder):** Python 3.x disponível no ambiente de benchmark
  com `pip install chromadb` (versão a pinar na task).

Entregue: `Competitor` "Chroma" pinado a `chromadb==1.5.9` em
`benches/src/competitors.rs`, gated por `--features compare-chroma`. O adapter
(`run_chroma`) invoca `benches/chroma_bench.py` como subprocess (protocolo JSON via
stdin/stdout, sem servidor/rede), alimentando os mesmos vetores pré-computados que
sqlite-vec/zvec recebem (Chroma nunca reembeda) e devolvendo os ids retornados por
query; o recall@10 é calculado do lado Rust contra o mesmo baseline brute-force, igual
aos outros dois adapters. Verificado ponta a ponta com `COMPARE="--features
compare-chroma" ./benches/run_all.sh agent-mem-10k` num ambiente com Python 3.14 +
chromadb 1.5.9 — linha do Chroma populada na tabela "vs. baselines" (recall@10 0.9936,
p50 0.70 ms, p99 1.29 ms, 19.7 MiB em disco), com escopo declarado (§4 regra 6).

---

## Fase FR — Frescor do conhecimento + observabilidade (pré-launch — decisão do founder 2026-07-10)

> Origem: dogfooding via Painel Agêntico — o EmbedMind virou a memória do agente que
> desenvolve o próprio EmbedMind, e três achados saíram do uso real: o ranking não tem
> componente temporal (memória defasada vence a correção nova), as relações
> `contradicts`/`refines` são só navegacionais (o recall não age sobre elas), e não há
> observabilidade de operações. Decisão do founder: entra ANTES do launch de 11/ago —
> **"conhecimento versionado" é diferencial de anúncio** que nenhum embarcado tem.
> Stories: S19–S22 da [spec](01-spec.md). Ordem interna: FR1 antes de FR3 (a resposta
> de near-dup sugere o fluxo supersedes e filtra superseded); FR2 e FR4 independentes.

### FR0. Docs ao estado real: README/ROADMAP refletindo M2/M3 entregues

O README diz "v0.1 vector-only, full-text/filtros next (M2)", mas S9/S10/S13/S14 estão
entregues — informação defasada nos docs já induziu erro em consumidores (comprovado em
10/jul). Atualizar README.md (linha de status, features, tabela de tools, claims de
roadmap) e ROADMAP.md (marcar 2.3/2.4/2.5 e 3.1/3.2 como ✅; registrar a fase FR como
direção pré-launch) ao estado real do código. Nada não-entregue prometido como pronto
(mesma regra do A4); benchmarks citados continuam vindo de `benches/results/`.

- **DoD:** nenhuma claim do README/ROADMAP contradiz o código; fase FR registrada no
  ROADMAP; quickstart continua copy-paste válido.
- **Verificação:** revisão cruzada README/ROADMAP vs. `crates/` (S9, S10, S13, S14
  citadas com evidência de código/teste); `cargo test --workspace` verde (nenhum
  código de produção muda nesta task).

### FR1. `supersedes` de primeira classe (story S19) [✅ ENTREGUE]

Semântica de versão de conhecimento: `remember(supersedes: [id])` exclui o alvo de todo
`recall` subsequente preservando-o como histórico navegável (`get` + `related` nos dois
sentidos). Reusar a infra de relações tipadas do grafo (C1); decisões de design
(representação flag-no-record vs. índice de exclusão; interação com forget/vacuum;
cadeias) registradas em ADR; FORMAT.md atualizado se houver campo/página novo (versão
aditiva, política G4); crash tests cobrindo as páginas tocadas.

- **DoD:** story S19 verde em core, MCP e CLI; ADR escrito; `vacuum` preserva
  superseded; sem regressão na suite.
- **Verificação:** `cargo test -p embedmind-core supersede` + testes de protocolo
  (`embedmind-mcp`) + E2E CLI + `cargo test --workspace`. ✅
- **Entrega (jul/2026):** flag no record (bit 1 de `flags`, sem bump de versão —
  [ADR 0013](adr/0013-supersedes-flag-no-record.md)) + aresta `"supersedes"` no grafo,
  mesma transação; exclusão re-verificada no registro em toda busca; `vacuum` preserva;
  `forget` do substituto não ressuscita; crash harness `crash_supersede.rs`; MCP
  `supersedes: [ids]`, CLI `--supersedes ID` repetível.

### FR2. Recência na fusão do recall (story S20) [✅ ENTREGUE]

Terceira lista na fusão RRF k=60: os candidatos de conteúdo (união vetor+texto)
reordenados por `created_at` decrescente — desempata pelo mais novo sem derrubar match
forte antigo (propriedade do RRF; ADR 0005 preservado, só posições de rank). Default
vs. opt-in decidido POR MEDIÇÃO no harness (limiar do BENCHMARKS.md §5) e registrado
em ADR com os números.

- **DoD:** story S20 verde; casos de ouro (a correção vence; o match forte antigo não
  é derrubado por novidade fraca); property tests da fusão de 3 listas; medição
  antes/depois anexada ao ADR.
- **Verificação:** `cargo test --workspace` + `benches/run_all.sh` nos dois datasets.

### FR3. Curadoria na escrita — near-duplicates no `remember` (story S21) — depende de FR1 [✅ ENTREGUE]

A resposta do `remember` ganha `similar: [{id, content truncado, score,
created_at_micros}]` acima de um limiar (decidido por medição no corpus do harness,
registrado em ADR), reusando o embedding já computado do próprio `remember` (zero
embedding extra). Gravação sempre acontece — informar, nunca bloquear. Depende de FR1:
a resposta sugere o fluxo supersedes e o filtro exclui superseded.

- **DoD:** story S21 verde em core, MCP e CLI; NFR `remember` p99 < 200 ms mantido
  (medido); primeira memória → `similar: []`.
- **Verificação:** `cargo test --workspace` + E2E MCP/CLI + `benches/run_all.sh`
  (p99 do remember).

### FR4. Op-log estruturado no `serve` (story S22) [✅ ENTREGUE]

`embedmind serve --op-log <path>` — 1 linha JSON (JSONL) por tool call: `{ts, tool,
args resumidos/truncados ~200 chars, ids, scores, latency_ms, project, isError}`.
Flag ausente = zero custo; falha de escrita do log nunca falha a tool (aviso em
stderr); stdout permanece exclusivo do protocolo. Consumidor imediato: card de memória
do Painel Agêntico (tail via SSE).

- **DoD:** story S22 verde; E2E MCP com `--op-log` validando cada linha como JSON
  independente, incluindo caso `isError: true`.
- **Verificação:** `cargo test --workspace` (E2E dos crates mcp/cli). ✅
- **Entrega (jul/2026):** módulo `oplog` no `embedmind-mcp` (`OpLog`: sink
  append-only + flush por linha, `McpServer::with_op_log`); flag `--op-log` no
  `embedmind serve` E no binário `embedmind-mcp`; `content`/`query` truncados a
  200 chars; erro de engine e erro de protocolo em tool despachada ambos logados
  com `isError: true` + campo `error`; falha de escrita só avisa em stderr
  (testado com sink que sempre falha); falha ao ABRIR o log é erro de startup.

### FR5. Relatório de uso — `embedmind report` (story S23) [✅ ENTREGUE]

`embedmind report [--op-log <path>] [--since N] [--json]` — a resposta de confiança
ao usuário ("a memória está sendo usada?"): sessões, recalls servidos, contadores
por memória (top recalladas + peso morto) e latências, agregados do op-log e
juntados com o arquivo. `--json` = primeira saída estruturada do CLI.

- **DoD:** story S23 verde; unit tests do agregador + E2E CLI/MCP.
- **Verificação:** `cargo test --workspace`. ✅
- **Entrega (jul/2026):** módulo `report` no `embedmind-mcp` (agregador ao lado do
  escritor do formato, `oplog`); linha `{tool:"session"}` no initialize do serve
  (conta sessões; testes de op-log atualizados para o novo contrato); subcomando
  `report` no CLI (join com o store via `iter()`/preview; degrada sem op-log).
  Decisão: contadores DERIVADOS do log, sem coluna nova no record — formato do
  .mind intocado, recall segue leitura pura.

---

## Fase FT — Otimização do full-text (pré-launch — decisão do founder 2026-07-11)

> Origem: a task BQ1 (`ef_search` escalonado) isolou que o NFR `recall p99 @ 100k
> < 50 ms` (medido 1.224,62 ms — 24x acima) NÃO vem da busca vetorial
> (`Store::recall_vector` mede 19,32 ms no mesmo run — dentro do orçamento) e sim do
> meio full-text da fusão híbrida: o scan de BM25 decodifica a postings list inteira
> de cada termo, sem corte antecipado, custo O(tamanho da lista) que domina a 100k.
> Ver [ADR 0017](adr/0017-otimizacao-do-full-text-escopo-e-metodo.md) para o escopo
> completo — **profiling obrigatório antes de qualquer otimização estrutural**, bump
> de `format_version` liberado quando a otimização exigir (aditivo, arquivo antigo
> continua legível). Stories: S23–S25 da [spec](01-spec.md). **Ordem estritamente
> sequencial** (ao contrário de BQ/FR): FT2 e FT3 dependem do resultado de FT1; nenhuma
> task de otimização deve ser derivada/executada antes do relatório de profiling
> existir — a derivação deve marcar FT2/FT3 com "DEPENDÊNCIA AUSENTE" se rodada fora
> de ordem.

### FT1. Profiling do meio full-text @ 100k (story S23)

Isolar, com evidência de profiling (não leitura de código), a fração do tempo do
`recall` híbrido @ 100k gasta em cada fase do full-text: decodificação de postings,
I/O de página, hashing do `HashMap<Ulid, f32>` de scores, recarga de registro pela
closure `keep`/`doc_len`. Ferramenta nativa da plataforma (flamegraph via
`perf`/`samply`/equivalente) sobre `agent-mem-100k` já aquecido; se profiling não
estiver disponível no ambiente, instrumentação manual com `Instant` ao redor de cada
fase de `fts::search` é aceitável — o que importa é o número, não a ferramenta.
Resultado registrado no ADR 0017 (seção "Resultado do profiling") ou arquivo próprio
referenciado por ele: números concretos por fase.

- **DoD:** story S23 verde — relatório existe, cita números concretos, e ou aponta
  uma fração dominante (> 60% do tempo do meio full-text) ou registra explicitamente
  "sem causa dominante clara" (resultado válido que muda a estratégia das próximas
  tasks — ver ADR 0017 §3).
- **Verificação:** revisão do relatório contra os números do harness
  (`query_engine_p50/p99_ms` menos `query_vector_p50/p99_ms` = o alvo do profiling,
  métrica introduzida no PR #9); `cargo test --workspace` verde (profiling não deve
  alterar comportamento de produção nesta task).
- **Atenção:** esta task é SOMENTE leitura/medição — nenhuma otimização entra aqui,
  mesmo que a causa pareça óbvia durante a investigação. Anotar candidatos para as
  próximas tasks, não implementar.

### FT2. Early termination no scan de postings (story S24) — depende de FT1

Cortar o scan de BM25 antes de decodificar/pontuar a postings list inteira de um
termo, quando já há candidatos suficientes para o top-k final. Critério exato do
corte (limiar de score, contagem de candidatos, ou equivalente) decidido por medição,
registrado em ADR novo (0018) que referencia o resultado do FT1. Sem mudança de
`format_version` — só o algoritmo de scan sobre a estrutura já existente. Resultado
DEVE ser byte-idêntico ao scan completo em qualquer regime (early termination só
reduz trabalho, nunca muda quais documentos retornam ou sua ordem).

- **Pré-requisito:** ler o relatório da FT1 antes de implementar. Se o profiling
  apontou outra causa dominante (ex.: I/O de página, não decodificação/scoring), esta
  task deve ser reavaliada antes de prosseguir — parar com "DEPENDÊNCIA AUSENTE" se o
  relatório não existir ou não sustentar esta abordagem.
- **DoD:** story S24 verde; teste de equivalência (resultado idêntico com/sem early
  termination, corpus pequeno e grande); ADR 0018 escrito com o critério de corte e a
  justificativa; `benches/run_all.sh --full` mostrando o ganho de
  `query_engine_p50/p99_ms` @ 100k.
- **Verificação:** `cargo test --workspace` + `benches/run_all.sh --full` (comparar
  contra o baseline pré-FT2, não sobrescrever sem registrar o antes/depois).

### FT3. Compressão delta+varint e/ou skip lists nas postings (story S25) — depende de FT1/FT2

Se o profiling (FT1) apontou I/O de página ou volume de bytes decodificados como
causa relevante, OU o early termination (FT2) sozinho não fechar o NFR
`recall p99 @ 100k < 50 ms`: comprimir `record_id` como delta+varint nas postings
(a lista já é ordenada por id) e/ou introduzir uma estrutura de skip (blocos de
tamanho fixo, pulável sem decodificar) dentro de uma postings list grande. Muda a
codificação de `FTS_POSTINGS` — bump de `format_version` (aditivo, ADR 0017 §2): um
arquivo de versão anterior continua legível pelo layout antigo, nunca erro.

- **Pré-requisito:** ler os relatórios/resultados de FT1 e FT2. Se FT2 já fechou o
  NFR, esta task pode ser adiada (registrar a decisão) — não é obrigatória por
  princípio, só se o NFR continuar reprovado.
- **DoD:** story S25 verde; round-trip de leitura/escrita entre `format_version`
  antigo e novo (arquivo antigo abre normalmente, sem skip list/compressão); crash
  test cobrindo as páginas `FTS_POSTINGS` no novo layout; `benches/run_all.sh --full`
  confirmando o NFR `recall p99 @ 100k < 50 ms` — OU, se ainda não fechar, ADR 0017
  atualizado com os números finais e a decisão do founder (prosseguir vs. aceitar
  limitação documentada).
- **Verificação:** `cargo test --workspace` incluindo fuzz do parser novo de
  `FTS_POSTINGS`; `benches/run_all.sh --full` nos dois datasets.

---

## Fase C — M3: profundidade — semanas 9–12

### C1. Camada de grafo simples (item 3.1) [✅ ENTREGUE]

Entidades + relações entre memórias, persistidas em páginas próprias (integradas a
WAL/crash tests): gravar relações explícitas no `remember`, navegar
`related(id | entity)`, expansão opcional de 1 salto no `recall`. O diferencial
vs. "só vetor" que nenhum embarcado tem completo.

- **DoD:** story S13 verde; formato das páginas de grafo especificado em FORMAT.md
  (versionado); relações somem com tombstone do alvo; fuzz target do parser novo;
  exposto nas cascas — MCP (`remember` com `entities`/`relations`, `recall` com
  `expand_related`, novo tool `related(id | entity)`) e CLI (`remember --entity/
  --relation`, `recall --expand-related`, subcomando `related`).
- **Verificação:** `cargo test -p embedmind-core graph` + fuzz smoke; testes de
  protocolo (`embedmind-mcp`) e end-to-end de CLI cobrindo o fluxo de grafo.

### C2. Proveniência básica exposta (item 3.2) [✅ ENTREGUE]

Filtro por agente no `recall` (`Query::agent`, mesmo predicado `keep` da B3) + breakdown
por agente vivo no `stats` (`StoreStats::by_agent`, com sessões distintas por agente),
exposto por MCP (`recall` ganha `agent`; novo tool read-only `stats`) e CLI
(`recall --agent`; seção "by agent" no `stats`). Atestação e histórico completo ficam
fora do escopo (decisão do founder, pós-tração).

- **DoD:** story S14 verde via MCP e CLI.
- **Verificação:** round-trip de proveniência em `embedmind-core` (api), `embedmind-mcp`
  (protocol) e `embedmind` (cli).

### C4. [MANUAL — founder] 3º post + go/no-go do dia 90 (itens 3.3, 3.4)

Caso de uso real com números do dogfooding; avaliação contra a tabela de métricas do
[00-prd.md](00-prd.md) §4 com as regras de decisão pré-comprometidas.

---

## Fase D — M4–M6: pós-90 dias (CONDICIONADO a GO no dia 90)

> Nenhuma task desta fase entra em execução antes do GO registrado pelo founder.

### D1. Extensões além do núcleo — decisão do founder pós-tração

Candidatas (guiadas pelas issues mais pedidas): criptografia at-rest (formato já
reservado — ADR 0007: AES-256-GCM por página, nonce page_no+epoch, KDF no header),
sync/equipe, time-travel/histórico, RBAC/auditoria. **Nenhuma entra sem decisão
explícita do founder** — se/como empacotá-las é discussão externa a este repo.

- **DoD:** definido junto com a escolha da extensão (novo ciclo de spec).
- **Verificação:** suite própria + crash tests estendidos se tocar o formato.

### D2. Vitrine da engine

App pequeno de notas/memória por voz 100% local — demonstra a engine a público
não-dev.

### D3. Bindings TypeScript (e Swift/C conforme demanda)

Mesmo padrão da B5; napi-rs ou WASM a decidir por ADR na hora.

---

## Marcos e critérios de go/no-go

| Marco | Critério | Data |
|---|---|---|
| **M1 fecha** | v0.1 fim a fim: binários de release + benchmark publicado + README final; `cargo test --workspace` verde nas 3 plataformas | pré-dia 35 |
| **Launch (M2)** | repo público + post + release v0.1.0 no ar | ≈ 11/ago/2026 (hard stop; dia 45 ainda privado = lançar o que existir) |
| **M2 fecha** | full-text + filtros + vacuum + bindings Python released | ≈ semana 8 |
| **M3 fecha / go-no-go** | grafo + proveniência no ar; decisão pela tabela do PRD §4 | ≈ 05/out/2026 |
| **M4+** | somente com GO explícito | pós-90 dias |
