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

### A4. README final de launch (item 1.7, parte dev do conteúdo)

Atualizar o README com a tabela de benchmark real (da A3), status v0.1 (remover o
aviso pre-v0.1), instruções de instalação por binário, e seção de comparação honesta
("quando usar sqlite-vec em vez de EmbedMind").

- **DoD:** README sem promessas não-entregues; benchmark com números reais e derrotas
  incluídas; quickstart validado literalmente (copy-paste funciona).
- **Verificação:** seguir o quickstart do zero numa máquina/pasta limpa.
- Nota: o GIF de demo de 30s e o teste com um 2º agente além do Claude Code são
  [MANUAL — founder]; a task prepara o roteiro do GIF (sequência de comandos a gravar).

### A5. [MANUAL — founder] Fechar o M1

Gravar o GIF, testar com 2º agente (Cursor ou outro), `cargo publish` real, criar o
GitHub Release v0.1.0. **Gate:** M1 completo = pré-condição do launch (dia 35).

---

## Fase B — M2: lançamento público + híbrido — semanas 5–8

### B1. [MANUAL — founder] Dia 35: repo público + lançamento coordenado (item 2.1)

Show HN, r/ClaudeAI, r/LocalLLaMA, r/rust, X. Post: *"I built persistent memory for
coding agents in Rust — single file, no server"*. Itens 2.2 (responder issues < 24h) e
2.7 (releases quinzenais guiados por issues) são processo contínuo do founder.

### B2. Full-text search na engine (item 2.3)

Índice invertido próprio nas páginas (default do DESIGN §12 — decidir vs. tantivy AQUI,
registrando ADR 0010), integrado ao WAL/transações, com BM25; fusão com o ranking
vetorial por RRF k=60 (ADR 0005) no `recall`.

- **DoD:** stories S9 da spec verdes (termo exato raro encontra a memória; fusão nunca
  exige interseção; arquivo antigo degrada para vetor-só com aviso); crash tests
  cobrindo as páginas novas; fuzz target para o parser do índice se houver formato
  novo; ADR 0010 escrito.
- **Verificação:** `cargo test --workspace` + casos de ouro da S9.

### B3. Filtros de metadados no `recall` (item 2.4)

`recall(query, filters: {...})` na API, MCP (extensão do schema da tool) e CLI;
semântica AND; mesma garantia anti-sub-retorno do ef_search adaptativo.

- **DoD:** story S10 verde; filtros compostos com escopo de projeto e tombstones;
  schema MCP atualizado de forma retrocompatível.
- **Verificação:** `cargo test -p embedmind-core filters` + E2E MCP.

### B4. `embedmind vacuum` real (v0.2, promessa registrada no CLI)

Reconstrução por cópia (nunca in-place): novo arquivo sem tombstones/overflow órfãos,
índices reconstruídos, troca atômica no final; crash em qualquer ponto preserva o
original.

- **DoD:** story S11 verde; crash test da varredura cobrindo o vacuum; `stats` antes/
  depois mostra redução.
- **Verificação:** teste round-trip ingest → forget 50% → vacuum → invariantes.

### B5. Bindings Python (item 2.5)

Crate `bindings/python` com PyO3 + maturin: `Store`, `remember`, `recall`, `forget`,
`stats` com a mesma semântica; wheels para as 3 plataformas no CI de release.

- **DoD:** `pip install` do wheel local funciona; suite pytest espelhando os E2E do
  CLI; mesmos arquivos `.mind` legíveis por Rust e Python.
- **Verificação:** `maturin build && pip install ... && pytest`.

### B6. [MANUAL — founder] 2º post técnico (item 2.6)

A engine por dentro (WAL, HNSW em arquivo único). Agente pode preparar rascunho
técnico com diagramas se solicitado — publicação é do founder.

---

## Fase C — M3: profundidade + funil premium — semanas 9–12

### C1. Camada de grafo simples (item 3.1)

Entidades + relações entre memórias, persistidas em páginas próprias (integradas a
WAL/crash tests): gravar relações explícitas no `remember`, navegar
`related(id | entity)`, expansão opcional de 1 salto no `recall`. O diferencial
vs. "só vetor" que nenhum embarcado tem completo.

- **DoD:** story S13 verde; formato das páginas de grafo especificado em FORMAT.md
  (versionado); relações somem com tombstone do alvo; fuzz target do parser novo.
- **Verificação:** `cargo test -p embedmind-core graph` + fuzz smoke.

### C2. Proveniência básica exposta (item 3.2)

Filtro por agente no `recall` (`filters` já existente da B3) + breakdown por agente no
`stats`; documentar como semente da rastreabilidade premium (fronteira: atestação e
histórico completo são pagos).

- **DoD:** story S14 verde via MCP e CLI.
- **Verificação:** teste round-trip de proveniência.

### C3. Página "Pro/Team — coming soon" (item 3.3, parte dev)

Página estática (no repo ou site simples) com a lista premium (histórico, compliance,
rastreabilidade, integrações) + captura de e-mail funcionando — **o instrumento de
validação de receita** do go/no-go.

- **DoD:** página publicada, captura testada, link no README.
- **Verificação:** submeter e-mail de teste e confirmar registro.

### C4. [MANUAL — founder] 3º post + go/no-go do dia 90 (itens 3.4, 3.5)

Caso de uso real com números do dogfooding; avaliação contra a tabela de métricas do
[00-prd.md](00-prd.md) §4 com as regras de decisão pré-comprometidas.

---

## Fase D — M4–M6: pós-90 dias (CONDICIONADO a GO no dia 90)

> Nenhuma task desta fase entra em execução antes do GO registrado pelo founder.

### D1. 1º módulo premium — o mais pedido na lista Pro

Provavelmente criptografia at-rest (formato já reservado — ADR 0007: AES-256-GCM por
página, nonce page_no+epoch, KDF no header) ou sync/equipe. Se houver demanda regulada:
tier Enterprise (RBAC, air-gap, auditoria, compliance LGPD/BACEN). **Fora do núcleo
MIT** — crate/repo separado conforme fronteira open-core do plano §8.

- **DoD:** definido junto com a escolha do módulo (novo ciclo de spec).
- **Verificação:** suite própria + crash tests estendidos se tocar o formato.

### D2. Vitrine da engine

App pequeno de notas/memória por voz 100% local — demonstra a engine a público
não-dev, testa 2ª fonte de receita.

### D3. Bindings TypeScript (e Swift/C conforme demanda)

Mesmo padrão da B5; napi-rs ou WASM a decidir por ADR na hora.

### D4. [MANUAL — founder] Licença comercial de embarque

Modelo SQLite/Realm (US$ 2–20k/ano por produto embarcante) — exige a engine graduada
como crate/lib independente (consequência natural da B5/D3).

---

## Marcos e critérios de go/no-go

| Marco | Critério | Data |
|---|---|---|
| **M1 fecha** | v0.1 fim a fim: binários de release + benchmark publicado + README final; `cargo test --workspace` verde nas 3 plataformas | pré-dia 35 |
| **Launch (M2)** | repo público + post + release v0.1.0 no ar | ≈ 11/ago/2026 (hard stop; dia 45 ainda privado = lançar o que existir) |
| **M2 fecha** | full-text + filtros + vacuum + bindings Python released | ≈ semana 8 |
| **M3 fecha / go-no-go** | grafo + proveniência + página Pro no ar; decisão pela tabela do PRD §4 | ≈ 05/out/2026 |
| **M4+** | somente com GO explícito | pós-90 dias |
