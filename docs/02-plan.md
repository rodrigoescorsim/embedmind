# Plano técnico — EmbedMind

> Documento canônico do pacote SDD (02 de 04). Fonte de verdade do **com quê**. Este
> plano condensa e aponta para os normativos do repo: [DESIGN.md](../DESIGN.md)
> (decisões de engenharia com justificativa — **consultar antes de implementar qualquer
> módulo**), [FORMAT.md](FORMAT.md) (spec byte a byte do `.mind`),
> [docs/adr/](adr/README.md) (decisões completas, uma por arquivo). Zero "a decidir":
> toda questão aberta tem default registrado; um agente nunca trava por falta de decisão.

## 1. Stack travada (com versões)

| Item | Versão | Papel |
|---|---|---|
| Rust | stable ≥ 1.90, edition 2024, **sem nightly** | linguagem única do workspace |
| `ort` | 2.0.0-rc.12 | ONNX Runtime CPU (embeddings) — baixa a lib no build (`download-binaries`) |
| `tokenizers` | 0.23 (features: onig) | tokenização HF, vocab embutido no binário |
| `xxhash-rust` | 0.8 (xxh3) | checksums de página/header |
| `ulid` | 1 | ids ordenáveis por tempo |
| `thiserror` | 2 | erros tipados da lib |
| `clap` | 4 (derive) | CLI |
| `serde_json` | 1 | **só cascas MCP/CLI** — o formato binário NUNCA usa serde |
| `proptest`, `cargo-fuzz` | dev | property tests e fuzzing |

**Proibições de dependência:** sem tokio/async em lugar nenhum (ADR 0009); sem crates de
rede no núcleo; fuzz é workspace separado (nightly + libFuzzer só no job Linux do CI).
Lints de workspace: `unsafe_code = forbid`; clippy `unwrap_used/expect_used/panic = deny`.
Release: LTO, 1 codegen-unit, strip.

## 2. Arquitetura e módulos

```
┌───────────────────────────────────────────────────────────┐
│ embedmind-cli            embedmind-mcp (stdio JSON-RPC)   │  cascas
├───────────────────────────────────────────────────────────┤
│ embedmind-core::api      — Memory, Store, Query (API pública)
│ embedmind-core::recall   — fusão híbrida de scores (RRF)  │
│ embedmind-core::index    — HNSW │ (M2: full-text, meta)   │
│ embedmind-core::embed    — pipeline ONNX (trait Embedder) │
│ embedmind-core::storage  — pager, WAL, page cache, B-tree │
│ embedmind-core::format   — layout binário, checksums      │  o ativo
└───────────────────────────────────────────────────────────┘
```

Regras de dependência: cascas dependem só de `api`; `embed` é plugável (`trait
Embedder`); `storage`/`format` não conhecem nada acima. **Zero lógica de domínio nas
cascas** — trocar MCP por outro protocolo = reescrever ~300 linhas. O CLI embute o
servidor (`embedmind serve` = mesmo servidor do crate `embedmind-mcp`): um binário
instalado cobre standalone + integração com agentes.

Crates futuros: `bindings/` — Python primeiro (PyO3/maturin, M2), TypeScript depois
(M4+), conforme demanda.

## 3. Modelo de dados

**Arquivo `.mind`** ([FORMAT.md](FORMAT.md) é a spec normativa): arquivo único paginado
(4 KiB, checksum xxh3 por página) + WAL sidecar transitório (`.mind-wal`, absorvido no
checkpoint — modelo SQLite). Header (página 0): magic `MINDFMT1`, `format_version`,
`page_size`, ponteiros (B-tree raiz, freelist, HNSW meta), metadados de embedding
(dims, model_id, quantização), flag + salt/KDF **reservados** para criptografia futura.
Little-endian fixo; todo (de)serialize explícito e fuzzável — nunca `repr(C)` cru.

**Record de memória:**

```rust
MemoryRecord {
    id: Ulid,                            // ordenável por tempo — timeline grátis
    content: String,
    embedding: VecRef,                   // ponteiro p/ bloco de vetor (f32 v0.1)
    metadata: BTreeMap<String, Scalar>,  // chaves livres, valores tipados
    project: Option<String>,             // escopo automático de projeto
    provenance: { agent, session_id?, created_at },  // básico grátis
    tombstone: bool,                     // forget = soft-delete + vacuum offline
}
```

B-tree de records: folhas slotted, nós internos de entrada fixa, split por ponto médio
de bytes comprovadamente seguro, cadeias de overflow para records > ~usable/4 (teto
32 MiB), scan em ordem. Sem delete físico — tombstone; overflow órfão espera o vacuum
(vazamento documentado).

## 4. Decisões de arquitetura (padrão ADR — completas em docs/adr/)

| # | Decisão | Alternativa rejeitada | Por quê |
|---|---|---|---|
| 0001 | WAL físico de páginas | log lógico de operações | recovery trivial e verificável por fuzzing |
| 0002 | HNSW próprio persistido em páginas | lib externa in-memory (hnsw_rs, usearch) | abrir 1 GB sem carregar tudo; o índice É o moat — não terceirizar |
| 0003 | `forget` = soft-delete + vacuum offline | remoção online no HNSW | complexidade não paga na v0.x |
| 0004 | Modelo embarcado (MiniLM-L6-v2 int8, ~23 MB, 384 dims) | exigir API de embedding | "no API key" é a promessa local-first |
| 0005 | RRF (k=60) para fusão híbrida | pesos aprendidos/calibrados | zero tuning, explicável |
| 0006 | Single-writer / multi-reader, sem MVCC | MVCC | um agente por arquivo é o caso real; lock de arquivo entre processos |
| 0007 | Criptografia reservada no formato, não implementada | implementar já | formato não quebra depois; feature é futura (pós-90 dias) |
| 0008 | HNSW com endereçamento direto de páginas | tabela node_id→página | meta page O(1) para sempre; insert toca O(M) páginas; sem teto de nós |
| 0009 | MCP stdio JSON-RPC direto, sem SDK | `rmcp` (SDK oficial) | evita tokio; superfície usada é minúscula (`initialize`, `ping`, `tools/list`, `tools/call`) |

**Questões abertas COM DEFAULT** (mudou de ideia → novo ADR 0010+):

- Full-text próprio nas páginas (**default**) vs. embutir tantivy — decidir no M2;
  tantivy quebra o modelo de página única e o WAL.
- Política fsync: só `full` na v0.1 (**default**); `batched` como opt-in futuro, nunca
  default.
- Quantização i8 de vetores: decidir no M3 **com o harness de benchmark** (4× menos
  espaço, ~1–2% de recall).
- Modelo multilíngue alternativo (bge-small etc.): decidir no dogfooding; troca é
  config via `trait Embedder`, não código.
- mmap vs. read/write + page cache próprio: **default read/write** (controle total de
  durabilidade; mmap complica WAL e Windows).
- Fase FR (2026-07-10): representação do `supersedes` (flag no record **default** vs.
  índice de exclusão), recência default vs. opt-in (S20) e limiar de near-duplicate
  (S21) — os três decididos POR MEDIÇÃO no harness e registrados em ADR (0012+),
  nunca por palpite.

## 5. Subsistemas — como cada um funciona

**Durabilidade (storage::wal + pager):** transação = append de páginas modificadas ao
WAL → fsync(wal) → commit record com checksum → visível. Checkpoint a 4 MB ou no
fechamento limpo. Recovery em toda abertura: aplica só transações com commit válido,
descarta cauda torta. Windows usa `FlushFileBuffers`; torn-write testado com o mesmo
harness. Toda I/O passa por `trait Vfs` (o truque do SQLite) — produção `RealVfs`,
testes `FaultVfs`/`storage::sim` com kill points, torn writes por setor e lying-fsync.

**Índice vetorial (index::hnsw):** HNSW paginado, `M=16`, `ef_construction=200`,
`ef_search=64` (por query). Distância coseno (vetores normalizados na inserção →
produto interno). Adjacências guardam `page_no` direto (ADR 0008). Seleção de vizinhos
com heurística de diversidade (Algoritmo 4 + keepPrunedConnections). Inserção
incremental dentro da transação (páginas do grafo entram no WAL). Tombstones filtrados
com `ef_search` adaptativo (×4/rodada até o grafo inteiro).

**Embeddings (embed):** `trait Embedder { embed, embed_chunks, id }`. Default
all-MiniLM-L6-v2 int8 via `ort`, tokenizer + modelo embutidos no binário. Chunking no
nível de índice: janelas de 510 tokens, overlap 64, teto 128 chunks/memória (acima →
erro tipado); cada chunk = 1 nó HNSW apontando para o mesmo record_id; busca dedup-a
por record (fica o melhor chunk). model_id + dims no header; modelo diferente na
abertura → recusa (caminho: `embedmind reembed`, futuro).

**Recall híbrido (recall):** v0.1 vetor + filtro de projeto/tombstone. M2: RRF k=60
entre lista vetorial e BM25. M3: expansão de grafo (1 salto) opcional.

**Casca MCP (embedmind-mcp):** stdio JSON-RPC direto. stdout exclusivo do protocolo;
logs em stderr. Erros de protocolo = códigos JSON-RPC; falhas de engine = tool result
com `isError: true`. Contexto de projeto: marcador mais próximo subindo do cwd —
`.embedmind.toml` (chave `project`) vence `.git` (nome da pasta raiz).

## 6. Contratos de API (schemas estáveis — são API pública)

**Tools MCP:**

| Tool | Entrada | Saída |
|---|---|---|
| `remember` | `content, metadata?, project?` (`null` = global) · fase FR: +`supersedes?: [ids]` (S19) | `{id, project}` · fase FR: +`similar: [...]` (S21) |
| `recall` | `query, limit?=8, project?, scope?, filters?` · fase FR: `recency?` SE a S20 decidir por opt-in | `{hits: [{id, content, score, project, provenance, created_at_micros}], scope}` |
| `forget` | `id` (query/idade + `confirm: true` no futuro) | `{count}` |

Fase FR na casca CLI: `remember --supersedes ID` (repetível) e `serve --op-log <path>`
(JSONL, S22) — schemas MCP evoluem de forma retrocompatível (campo novo, nunca
mudança de significado).

**CLI:** `embedmind serve | remember | recall | forget | stats | vacuum`, flags globais
`--file` (default `~/.embedmind/memory.mind`), `--project`/`--global`/`--all`,
`--limit`. Erros em stderr, exit code ≠ 0.

**API Rust (embedmind-core::api):** `Store::{create, open, open_or_create, remember,
get, recall, forget, iter, iter_all, stats, close}` + `MemoryDraft`, `Query`,
`Scope::{Project, All}`, `Recalled`, `StoreStats`. Injeção de `Vfs` custom para testes
e embarcadores.

## 7. Estratégia de testes (o moat operacionalizado — [TESTING.md](TESTING.md))

1. **Crash tests determinísticos** — varredura de todos os pontos de injeção × 4
   workloads; invariantes I1–I5 contra modelo de referência; tupla `(W, P, mode, seed)`
   reproduz qualquer falha. Roda em `cargo test` nas 3 plataformas.
2. **Fuzzing** — 5 alvos (`fuzz_header`, `fuzz_page`, `fuzz_record`, `fuzz_wal_replay`,
   `fuzz_open_full`); corpus versionado; CI: passe curto (2 min/alvo) por PR + noturno
   1h/alvo; corpos rodam como smoke tests estáveis em `cargo test`.
3. **Property tests** — engine real vs. modelo em memória (HashMap + busca linear);
   recall comparado por conjunto (HNSW é aproximado).
4. **Benchmark harness** ([BENCHMARKS.md](BENCHMARKS.md)) — datasets fixos
   (`agent-mem-10k/-100k` + subset público com labels), métricas recall@10, p50/p99
   (incl. cold-open), throughput, tamanho, RSS; vs. sqlite-vec, zvec e brute-force;
   dobra como guarda de regressão no CI.

CI (GitHub Actions): `cargo fmt --all --check` → `cargo clippy --workspace
--all-targets -- -D warnings` → `cargo test --workspace` em matriz Windows/Linux/macOS
+ job de fuzz Linux.

## 8. Fronteira de escopo do núcleo (não cruzar sem decisão do founder)

Núcleo: engine completa, MCP, CLI, vetor+texto+grafo, proveniência básica.
Fora do escopo (NUNCA implementar sem decisão explícita do founder): time-travel/
timeline, criptografia at-rest (formato já reservado), RBAC, trilha de auditoria,
atestação de proveniência, sync de equipe/conectores. Na dúvida sobre uma feature:
se serve ao dev solo local, é núcleo; se serve a time/compliance, fica fora por ora.
