# DESIGN.md — Documento de Design Técnico do EmbedMind

Documento interno de engenharia. Complementa o [ROADMAP.md](ROADMAP.md) (*o quê/quando*) respondendo *como*. Decisões marcadas **[DECIDIDO]** têm justificativa registrada; **[ABERTO]** são questões a resolver durante o M1, com a opção default indicada.

---

## 1. Objetivo e escopo da v0.1

Uma engine de memória embarcada, in-process, num único arquivo crash-safe, exposta como servidor MCP + CLI.

**Dentro do escopo v0.1 (M1):** arquivo único + WAL, KV store, busca vetorial HNSW, embeddings ONNX embarcados, MCP `remember`/`recall`/`forget`, contexto automático de projeto, CLI mínimo.

**Fora do escopo v0.1 (não implementar antes da hora):** full-text (M2), filtros de metadados (M2), grafo (M3), criptografia/RBAC (premium, pós-90), sync (premium), compactação online, multi-processo escritor.

**Requisitos não-funcionais que governam tudo:**

| Requisito | Alvo |
|---|---|
| Durabilidade | Nenhuma perda de memória confirmada, mesmo com kill -9 / queda de energia no meio da escrita |
| Integridade | Arquivo **nunca** fica em estado irrecuperável; recovery automático na abertura |
| Latência de `recall` | < 50 ms p99 para 100k memórias em hardware comum, CPU-only |
| Latência de `remember` | < 200 ms p99 (dominada pelo embedding, não pelo storage) |
| Pegada | Binário < 40 MB (incl. modelo de embedding); RAM < 300 MB para 100k memórias |
| Portabilidade | Windows, Linux, macOS com o mesmo formato de arquivo (endianness fixa, little-endian) |

---

## 2. Arquitetura em camadas

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

Regras de dependência: cascas dependem só de `api`; `embed` é plugável (trait); `storage`/`format` não conhecem nada acima. A engine compila `#![forbid(unsafe_code)]` exceto no módulo do mmap (se usado), isolado e auditável.

---

## 3. Formato de arquivo `.mind`

**[DECIDIDO] Arquivo único paginado + WAL como arquivo sidecar temporário** (`memory.mind` + `memory.mind-wal` durante operação; o WAL é absorvido no checkpoint — modelo SQLite). Justificativa: "um arquivo" é a promessa de produto; o sidecar transitório é aceitável (SQLite treinou o mercado) e simplifica radicalmente o recovery vs. WAL embutido no próprio arquivo.

### 3.1 Layout

```
┌──────────────────────────────────────────────┐
│ Header (página 0, 4 KiB)                     │
│  magic "MINDFMT1" · format_version u32       │
│  page_size u32 (default 4096)                │
│  root_btree_page · freelist_page             │
│  hnsw_meta_page · embedding_meta             │
│  (dims u16, model_id, quantização)           │
│  flags (bit reservado: encrypted)            │
│  salt/kdf reservados p/ criptografia futura  │
│  checksum do header (xxh3)                   │
├──────────────────────────────────────────────┤
│ Páginas de dados (B-tree de records)         │
│ Páginas de índice HNSW (nós + camadas)       │
│ Páginas de vetores (blocos alinhados)        │
│ Freelist                                     │
└──────────────────────────────────────────────┘
```

- **Página 4 KiB** com checksum xxh3 por página (detecção de corrupção silenciosa — pré-requisito da marca "nunca corrompeu").
- **Endianness little-endian fixa**; campos de tamanho variável com length-prefix. Nada de `repr(C)` cru serializado — todo (de)serialize explícito e fuzzável.
- **`format_version` com política escrita no código:** versão maior desconhecida → abre somente-leitura ou recusa com mensagem clara; migrações sempre via `embedmind migrate` (cópia, nunca in-place destrutivo).
- **Criptografia (premium) reservada no formato desde o dia 1:** flag + salt/KDF no header e páginas cifráveis individualmente (AES-256-GCM por página, nonce derivado de page_no + epoch). Não implementar agora; **não** ter que quebrar o formato depois.

### 3.2 Modelo de dados (record de memória)

```rust
MemoryRecord {
    id: Ulid,                    // ordenável por tempo — grátis p/ timeline
    content: String,             // o texto da memória
    embedding: VecRef,           // ponteiro p/ bloco de vetor (f32 ou i8 quantizado)
    metadata: BTreeMap<String, Scalar>,  // chaves livres, valores tipados
    project: Option<String>,     // escopo (ver §7)
    provenance: Provenance {     // básico grátis (semente do premium)
        agent: String,           // "claude-code", "cursor", "cli", ...
        session_id: Option<String>,
        created_at: DateTime,
    },
    tombstone: bool,             // forget = soft delete + vacuum posterior
}
```

**[DECIDIDO] `forget` é soft-delete (tombstone) + vacuum offline** (`embedmind vacuum`). HNSW não suporta remoção barata; tombstones filtram no recall e o vacuum reconstrói o índice. Honesto e simples; remoção online é otimização futura.

---

## 4. Durabilidade: WAL, checkpoint e recovery

**[DECIDIDO] WAL físico de páginas (page-level redo log), estilo SQLite — não log lógico.** Justificativa: recovery trivial e verificável por fuzzing (reaplicar páginas íntegras, descartar cauda inválida); log lógico exige replay de operações e multiplica estados possíveis — inaceitável para o requisito nº 1.

- **Escrita:** toda transação anexa páginas modificadas ao WAL → `fsync(wal)` → commit record com checksum → só então visível.
- **Checkpoint:** ao atingir limiar (default 4 MB) ou no fechamento limpo: páginas do WAL copiadas ao `.mind`, `fsync(main)`, WAL truncado.
- **Recovery na abertura:** varre o WAL, aplica só transações com commit record válido, descarta cauda torta (torn write). Sempre automático, sempre silencioso, sempre logado.
- **Política de fsync [ABERTO]:** default `full` (fsync a cada commit). Avaliar modo `batched` (fsync a cada N ms) como opt-in para workloads de ingestão — nunca como default.
- **Windows:** `FlushFileBuffers` no lugar de fsync; testar torn-write com o mesmo harness (é o dogfooding-onde-ninguém-testa citado na estratégia).

### Concorrência

**[DECIDIDO] Single-writer / multi-reader, em processo.** Escritas serializadas por um lock interno; leituras via snapshot (page cache copy-on-write leve). Multi-processo: lock de arquivo (advisory + `LockFileEx` no Windows) — segundo processo escritor recebe erro claro, leitura concorrente permitida. MVCC completo é não-objetivo da v0.x.

---

## 5. Índice vetorial (HNSW)

**[DECIDIDO] HNSW próprio, persistido em páginas — não biblioteca externa in-memory.** Justificativa: as libs prontas (hnsw_rs, usearch) assumem grafo em RAM e serialização monolítica; a promessa do produto é abrir arquivo de 1 GB sem carregar tudo. O HNSW é ~800 linhas bem testáveis; é exatamente a barreira técnica que É o moat — não terceirizar o moat.

- Parâmetros default: `M=16`, `ef_construction=200`, `ef_search=64` (ajustável por query). Distância: coseno (vetores normalizados na inserção → produto interno).
- **Layout — endereçamento direto de páginas (ADR 0008):** nós do grafo em páginas próprias; vizinhanças como arrays de `page_no: u64` apontando direto para as páginas dos vizinhos — **sem tabela node_id → página**. A meta page tem tamanho fixo para sempre; insert toca O(M) páginas independentemente do tamanho do índice; um hop de busca = uma leitura de página. Camadas superiores (pequenas) podem residir integralmente em cache (otimização futura).
- **Seleção de vizinhos com heurística de diversidade** (Algoritmo 4 do paper HNSW, como hnswlib/faiss), com `keepPrunedConnections` — recall melhor em dados clusterizados (embeddings de texto) sem custo de formato.
- **Inserção incremental** dentro da transação (as páginas tocadas do grafo entram no WAL como quaisquer outras).
- **Vetores:** f32 no v0.1; **[ABERTO]** quantização i8 (SQ) como opção de build do índice no M3 — 4× menos espaço, perda de recall ~1–2%, decidir com o harness de benchmark.
- Tombstones filtrados na busca; se o filtro deixar o resultado incompleto, `ef_search` cresce adaptativamente (×4 por rodada, teto = node_count) até preencher ou esgotar o grafo — degrada rumo a scan honesto até o vacuum, nunca sub-retorna em silêncio.

## 6. Embeddings

**[DECIDIDO] Modelo embarcado default + trait para BYO.**

- Runtime: crate `ort` (ONNX Runtime, CPU). Modelo default: **all-MiniLM-L6-v2 quantizado int8** (~23 MB, 384 dims) — o melhor custo/qualidade para memória semântica curta; multilíngue razoável (importante: founder pt-BR).
- **[ABERTO]** avaliar `bge-small` ou modelo multilíngue dedicado se o recall em português decepcionar no dogfooding. A troca é config, não código (`trait Embedder { fn embed(&self, text: &str) -> Vec<f32>; fn id(&self) -> ModelId; }`).
- O `model_id` + dims ficam gravados no header; **misturar embeddings de modelos diferentes no mesmo arquivo é erro** — trocar de modelo exige `embedmind reembed` (que é também o caminho de upgrade quando modelos melhorarem: já previsto como feature premium de histórico/reprocessamento).
- Tokenização: `tokenizers` (HF) com o vocab embutido no binário. **Chunking (nível de índice, não de registro):** memórias > 510 tokens de conteúdo são divididas em janelas de 510 tokens com overlap de 64 (`Embedder::embed_chunks`); cada chunk vira **um nó a mais no HNSW apontando para o mesmo `record_id`** — o registro permanece inteiro, sem `parent_id` nem registros-filho. A busca dedup-a por `record_id` (fica o melhor chunk) e devolve a memória inteira, nunca o chunk. `vec_ref` do registro aponta para o vetor do primeiro chunk. Teto: 128 chunks por memória (~57k tokens) — acima disso `remember` falha com erro tipado em vez de indexar visão truncada. Queries usam `embed` simples (truncam na primeira janela — queries são curtas por natureza).

## 7. Recall híbrido e escopo de projeto

- **v0.1:** só vetor + filtro de projeto + tombstone.
- **M2 (full-text + metadados):** fusão por **Reciprocal Rank Fusion** (RRF, k=60) entre lista vetorial e lista BM25 — sem pesos mágicos a calibrar, comportamento explicável. Full-text: **[ABERTO]** índice invertido próprio nas páginas (default; consistente com "tudo num arquivo") vs. embutir tantivy (mais rápido de entregar, mas quebra o modelo de página única e o WAL).
- **Escopo de projeto:** o servidor MCP infere o projeto do `cwd` do agente (raiz git ou config `.embedmind.toml`); `recall` filtra por projeto por default com fallback global explícito (`scope: "all"`). É a feature "memória automática de contexto de projeto" do M1 — barata na engine, enorme em UX.

## 8. Camada MCP

- Transporte **stdio JSON-RPC** (o denominador comum dos hosts MCP hoje). **[DECIDIDO — ADR 0009] Implementação direta do protocolo, sem SDK:** o subconjunto necessário (`initialize`, `ping`, `tools/list`, `tools/call`) é minúsculo e o `rmcp` traria tokio + stack async para um servidor síncrono de um cliente por processo. Única dependência nova: `serde_json`. Logs em stderr; stdout é canal exclusivo do protocolo.
- Tools expostas (schemas estáveis — são API pública). Estado v0.1:
  - `remember(content, metadata?, project?)` → `{id, project}` — `project` omitido = contexto detectado (item 1.5); `null` explícito = memória global.
  - `recall(query, limit?=8, project?, scope?)` → `{hits: [{id, content, score, project, provenance, created_at_micros}], scope}` — escopo default = projeto detectado; `scope: "all"` é o fallback global explícito; o escopo aplicado é ecoado na resposta. `filters` (metadados) chega no M2.
  - `forget(id)` → `{count}` — v0.1 só por id. `forget` por query/idade (com `confirm: true` obrigatório — agentes erram; deleção em massa não pode ser acidente de um tool call) chega quando a engine tiver deleção endereçada por query.
- A casca MCP contém **zero** lógica de domínio: parse → chamada da API `embedmind-core` → serialize. Trocar MCP por outro protocolo = reescrever ~300 linhas.
- O CLI `embedmind serve` roda o mesmo servidor (o crate `embedmind-cli` depende de `embedmind-mcp`); um único binário instalado cobre uso standalone e a integração com agentes.

## 9. Estratégia de testes (o moat operacionalizado)

1. **Crash tests determinísticos:** harness que roda workloads e mata o processo em pontos injetados (antes/depois de cada fsync, no meio de escrita de página — via camada de I/O mockável `trait Vfs`, como o SQLite); reabre e verifica invariantes (toda transação confirmada presente, nenhuma meia-transação, checksums OK). Roda no CI em toda plataforma.
2. **Fuzzing (`cargo-fuzz`):** alvos = parser do header, parser de página, replay de WAL, deserialização de record. Corpus versionado no repo.
3. **Property tests (`proptest`):** modelo de referência em memória (HashMap + busca linear) vs. engine real — mesmas operações, mesmos resultados (recall vetorial comparado por conjunto com tolerância, pois HNSW é aproximado).
4. **Benchmark harness público** (M1, item 1.7): dataset fixo, métricas de recall@k, latência, tamanho de arquivo, RAM — vs. sqlite-vec e zvec. O harness é também o guarda de regressão de performance no CI.

## 10. Dependências (crates) — orçamento deliberadamente curto

| Crate | Papel | Nota |
|---|---|---|
| `ort` + `tokenizers` | embeddings | as duas maiores; isoladas atrás do trait |
| `xxhash-rust` | checksums | |
| `ulid` | ids | |
| `serde`/`serde_json` | MCP e CLI apenas | o formato binário NÃO usa serde |
| `thiserror` | erros da lib | |
| `clap` | CLI | |
| `proptest`, `cargo-fuzz` | dev/teste | |

Sem tokio em lugar nenhum do workspace (I/O síncrono; o servidor MCP stdio é implementado direto, sem SDK — ADR 0009).

## 11. Decisões registradas (mini-ADRs)

> Versões completas (contexto, alternativas, consequências) em [docs/adr/](docs/adr/README.md) — um arquivo por decisão. Questões do §12, quando resolvidas, viram ADRs novos (0009+). A tabela abaixo é o resumo.

| # | Decisão | Alternativa rejeitada | Por quê |
|---|---|---|---|
| 1 | WAL físico de páginas | log lógico de operações | recovery verificável > flexibilidade |
| 2 | HNSW próprio paginado | lib externa in-memory | abrir sem carregar tudo; o índice é o moat |
| 3 | Soft-delete + vacuum | remoção online no HNSW | complexidade não paga na v0.x |
| 4 | Modelo embarcado (MiniLM int8) | exigir API de embedding | "no API key" é a promessa local-first |
| 5 | RRF para fusão híbrida | pesos aprendidos/calibrados | zero tuning, explicável, bom o suficiente |
| 6 | Single-writer | MVCC | um agente/usuário por arquivo é o caso real |
| 7 | Criptografia reservada no formato, não implementada | implementar já | formato não quebra depois; feature é premium |
| 8 | HNSW com endereçamento direto de páginas (sem tabela de localização) | tabela node_id→página na meta (encadeada) | meta O(1) para sempre; insert O(M); sem teto de nós |
| 9 | MCP stdio JSON-RPC direto, sem SDK | `rmcp` (SDK oficial) | evita tokio/async; superfície usada é minúscula; casca continua substituível |

## 12. Questões em aberto (resolver no M1, com default)

- [x] SDK MCP `rmcp` vs. protocolo direto → **resolvido: direto (ADR 0009)** — o SDK traz tokio+peso, confirmando o default
- [ ] Full-text próprio vs. tantivy (decisão só no M2) → *default: próprio*
- [ ] Política fsync `batched` opt-in → *default: só `full` na v0.1*
- [ ] Quantização i8 de vetores → *decidir com benchmark no M3*
- [ ] Modelo multilíngue alternativo → *decidir no dogfooding, semanas 2–4*
- [ ] mmap vs. read/write com page cache próprio → *default: read/write + cache próprio (controle total de durabilidade; mmap complica WAL e Windows)*
