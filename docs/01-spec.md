# Spec comportamental — EmbedMind

> Documento canônico do pacote SDD (01 de 04). Fonte de verdade do **o quê** — comporta-
> mento observável, nunca organização de código. Cada acceptance criterion é verificável
> por comando ou teste. Escrito em 08/jul/2026 contra o estado real do repo: stories
> marcadas **[✅ implementada]** já têm código e testes em `main` — o agente NÃO deve
> reimplementá-las, apenas mantê-las verdes. Specs normativas complementares:
> [FORMAT.md](FORMAT.md) (formato `.mind` byte a byte) e [TESTING.md](TESTING.md)
> (harness de crash, fuzzing, invariantes I1–I5).

## Convenções desta spec

- Ator "agente" = host MCP conectado (Claude Code, Cursor, custom); ator "usuário" =
  humano no CLI.
- Toda latência citada assume hardware comum, CPU-only, arquivo com 100k memórias,
  salvo indicação.
- Erros são **tipados e nunca derrubam o processo servidor**: falha de engine durante
  tool call vira resultado com `isError: true`; erro de protocolo vira código JSON-RPC.

---

## P0 — núcleo v0.1 (M1)

### S1. Lembrar uma memória — `remember` [✅ implementada]

Como agente, quero gravar um fato com metadados para reencontrá-lo semanticamente em
sessões futuras.

- **Dado** um arquivo `.mind` aberto, **quando** chamo `remember(content, metadata?,
  project?)`, **então** recebo `{id, project}` com id ULID (ordenável por tempo) e a
  memória está imediatamente visível para `recall` e durável (fsync no commit).
- **Dado** `project` omitido, **quando** gravo, **então** o projeto detectado do cwd é
  estampado; `project: null` explícito grava memória global.
- **Dado** conteúdo > 510 tokens, **quando** gravo, **então** o texto é indexado em
  janelas de 510 tokens com overlap de 64; o registro permanece inteiro e único;
  `recall` devolve a memória completa, nunca um chunk.
- **Borda:** conteúdo > 128 chunks (~57k tokens) → erro tipado, nada é indexado
  parcialmente. Conteúdo vazio → erro tipado. Metadado com valor não-escalar → erro
  tipado.
- **Borda:** proveniência básica (agent, session_id?, created_at) é estampada em toda
  memória; via MCP o `clientInfo.name` do handshake vira o agente.
- **Verificação:** `cargo test -p embedmind-core api::` e teste E2E do CLI.

### S2. Buscar semanticamente — `recall` [✅ implementada]

Como agente, quero buscar por significado (não keyword) e receber os melhores hits com
score, escopados ao projeto em que estou.

- **Dado** memórias gravadas, **quando** chamo `recall(query, limit?=8, project?,
  scope?)`, **então** recebo `hits: [{id, content, score, project, provenance,
  created_at_micros}]` ordenados por similaridade (coseno), melhor primeiro, e o escopo
  efetivamente aplicado é ecoado na resposta.
- **Dado** que estou num diretório de projeto, **quando** chamo sem `project`/`scope`,
  **então** só memórias do projeto detectado (+ nenhuma global de outro projeto)
  retornam; `scope: "all"` é a saída explícita para busca global.
- **Dado** memórias tombstoned (esquecidas), **quando** busco, **então** elas nunca
  aparecem — o status é re-verificado contra o registro no momento da busca, nunca
  confiado ao grafo.
- **Borda:** filtro deixa resultado incompleto → `ef_search` cresce adaptativamente
  (×4 por rodada, teto = grafo inteiro) — degrada rumo a scan honesto, nunca
  sub-retorna em silêncio.
- **Borda:** arquivo vazio → `hits: []`, não erro. `limit: 0` → lista vazia.
- **NFR:** p99 < 50 ms com 100k memórias (CPU-only, cache quente).
- **Verificação:** `cargo test -p embedmind-core recall` + property tests vs. modelo
  de referência linear.

### S3. Esquecer — `forget` [✅ implementada]

Como agente, quero apagar uma memória específica por id.

- **Dado** um id existente, **quando** chamo `forget(id)`, **então** recebo
  `{count: 1}` e a memória some de todo `recall` subsequente (soft-delete/tombstone;
  espaço é recuperado pelo `vacuum` offline).
- **Borda:** id inexistente ou já esquecido → `{count: 0}`, idempotente, zero bytes
  escritos. Id malformado → erro tipado.
- **Futuro (fora da v0.1):** `forget` por query/idade exigirá `confirm: true`
  obrigatório — deleção em massa não pode ser acidente de um tool call.
- **Verificação:** `cargo test -p embedmind-core forget`.

### S4. Durabilidade e integridade (a promessa da marca) [✅ implementada]

Como usuário, confio que nenhuma memória confirmada se perde e que o arquivo nunca fica
irrecuperável — mesmo com `kill -9` ou queda de energia no meio da escrita.

- **Dado** um crash em QUALQUER ponto de I/O (antes/depois de cada fsync, escrita
  parcial de página, torn write em granularidade de setor), **quando** reabro o
  arquivo, **então** o recovery roda automático e silencioso: toda transação com commit
  válido está presente (I-durabilidade), nenhuma meia-transação é visível
  (I-atomicidade), todos os checksums batem (I-integridade).
- **Dado** um WAL com cauda torta, **quando** abro, **então** o prefixo commitado é
  aplicado e a cauda inválida descartada sem intervenção.
- **Dado** hardware que mente no fsync, **então** integridade se mantém sempre;
  durabilidade dos últimos commits pode se perder (mesma postura do SQLite,
  documentada).
- **Borda:** segundo processo escritor no mesmo arquivo → erro claro imediato
  (lock advisory; `LockFileEx` no Windows); leitores concorrentes permitidos.
- **Verificação:** `cargo test --workspace` (harness `tests/crash.rs` +
  `tests/crash_records.rs` roda a varredura completa de pontos de injeção; falha
  imprime tupla reproduzível).

### S5. Arquivo hostil ou incompatível nunca derruba nem confunde [✅ implementada]

- **Dado** um arquivo corrompido/adversarial, **quando** abro ou decodifico qualquer
  página/record/WAL, **então** recebo erro tipado — nunca panic, nunca alocação
  desmedida (todo length-prefix é validado antes de alocar).
- **Dado** `format_version` maior que o suportado, **então** recusa com mensagem clara
  (política G4); flag `encrypted` ligada → recusa tipada (criptografia é premium
  futuro).
- **Dado** um arquivo gravado com outro modelo de embedding (model_id/dims diferentes
  no header), **quando** abro, **então** recusa tipada — misturar embeddings é erro;
  o caminho é `embedmind reembed` (futuro).
- **Verificação:** 5 alvos de fuzzing (`fuzz_header`, `fuzz_page`, `fuzz_record`,
  `fuzz_wal_replay`, `fuzz_open_full`) verdes; corpos também rodam como smoke tests em
  `cargo test` em toda plataforma.

### S6. Servidor MCP plug-and-play [✅ implementada]

Como usuário, adiciono uma linha no meu agente e ele ganha memória.

- **Dado** `claude mcp add embedmind -- embedmind serve --file <path>`, **quando** o
  host inicia a sessão, **então** o handshake `initialize`/`ping`/`tools/list` responde
  com as 3 tools de schema estável; stdout é canal exclusivo do protocolo (logs em
  stderr).
- **Dado** o cwd do agente dentro de um repo git ou de uma pasta com `.embedmind.toml`
  (chave `project`), **então** o marcador mais próximo subindo a árvore define o
  projeto (toml vence git); `remember`/`recall` usam esse contexto por default.
- **Verificação:** teste E2E que dirige uma sessão MCP completa via pipes stdio contra
  o binário real.

### S7. CLI standalone [✅ implementada]

- **Dado** o binário instalado, **quando** rodo `embedmind remember|recall|forget|
  stats|vacuum|serve` (com `--file`, `--project`, `--global`, `--all`, `--limit`),
  **então** cada comando opera sobre `~/.embedmind/memory.mind` por default com a mesma
  semântica das tools MCP; `stats` reporta contagens, layout do arquivo, entradas de
  índice e o modelo de embedding gravado.
- **Borda:** `embedmind vacuum` reconstrói por cópia e imprime o tamanho antes → depois
  e a contagem recuperada (S11). Erros saem em stderr com exit code ≠ 0.
- **Verificação:** testes E2E do crate `embedmind-cli` sobre o binário real.

### S8. Instalação em 1 comando [◔ parcial — falta release]

- **Dado** `cargo install embedmind` OU download de binário pré-compilado do Releases,
  **então** um único binário funciona em Windows, Linux e macOS sem dependência externa
  (modelo ONNX + tokenizer embutidos; ONNX Runtime resolvido no build).
- **Dado** o mesmo arquivo `.mind` copiado entre plataformas, **então** abre idêntico
  (little-endian fixo).
- **Pendente:** publicação no crates.io + binários de release no GitHub + teste manual
  com Claude Code **e mais 1 agente** (hoje só o build por fonte funciona).
- **NFR:** artefato de release comprimido < 40 MB incluindo modelo (~20 MiB hoje; o
  binário nu linka ONNX Runtime estático e passa de 40 MB — ver [ADR 0010](adr/0010-teto-de-tamanho-governa-artefato-comprimido.md));
  RAM < 300 MB para 100k memórias.
- **Verificação:** CI de release produz artefatos para as 3 plataformas; smoke test de
  instalação documentado.

---

## P1 — M2 (lançamento + híbrido)

### S9. Busca híbrida full-text + vetor [⬜ pendente]

- **Dado** memórias com termos exatos (nomes de função, siglas, ids), **quando** chamo
  `recall`, **então** a lista final funde ranking vetorial e BM25 por **Reciprocal Rank
  Fusion (k=60)** — um termo exato raro encontra a memória mesmo quando o embedding
  não aproxima.
- **Dado** só match vetorial ou só match textual, **então** o hit ainda aparece
  (a fusão nunca exige interseção).
- **Borda:** índice full-text ausente em arquivo antigo → recall degrada para
  vetor-só com aviso, nunca erro.
- **Verificação:** testes de fusão com casos de ouro (termo raro, sinônimo semântico,
  ambos) + property tests.

### S10. Filtros de metadados no recall [✅ implementada]

- **Dado** memórias com metadados tipados, **quando** chamo `recall(query,
  filters: {chave: valor|faixa})`, **então** só memórias que satisfazem TODOS os
  filtros retornam, com a mesma garantia anti-sub-retorno da S2.
- **Borda:** filtro por chave inexistente → 0 hits, não erro; tipo incompatível →
  erro tipado.
- **Verificação:** `cargo test -p embedmind-core filters`.

### S11. Vacuum — recuperar espaço [✅ implementada]

- **Dado** memórias esquecidas acumuladas, **quando** rodo `embedmind vacuum`,
  **então** tombstones e cadeias de overflow órfãs são removidos, o índice HNSW é
  reconstruído sem os mortos, e o arquivo resultante é menor ou igual; crash no meio
  do vacuum nunca perde o arquivo original (cópia, nunca in-place destrutivo).
- **Verificação:** teste de round-trip (ingest → forget 50% → vacuum → invariantes +
  tamanho reduzido) + crash test do vacuum.

### S12. Bindings Python [⬜ pendente]

- **Dado** `pip install embedmind`, **quando** uso `Store` do Python, **então**
  remember/recall/forget/stats funcionam com a mesma semântica (mesmos arquivos `.mind`
  compatíveis), destravando LangChain e agentes custom.
- **Verificação:** suite pytest dos bindings no CI.

---

## P2 — M3+ (profundidade e funil)

### S13. Camada de grafo — entidades e relações [⬜ pendente, M3]

- **Dado** memórias que mencionam entidades, **quando** gravo com
  `entities`/`relations` explícitas (extração automática NÃO é escopo desta story),
  **então** posso navegar `related(id | entity)` e o `recall` pode expandir 1 salto
  para puxar contexto conectado.
- **Borda:** relação para memória esquecida some junto do tombstone.
- **Verificação:** testes de grafo (inserção, navegação, expansão em recall).

### S14. Proveniência básica exposta [◔ dados já gravados; falta expor]

- **Dado** memórias gravadas por agentes distintos, **quando** consulto (`recall` já
  devolve `provenance`; `stats`/filtros por agente no M3), **então** vejo qual
  agente/sessão gravou o quê — grátis, semente da rastreabilidade premium.
- **Verificação:** teste de round-trip de proveniência via MCP e CLI.

### S15. Não-regressão de performance [contínua]

- **Dado** qualquer mudança na engine, **quando** o CI roda o harness de benchmark,
  **então** recall@10, latências p50/p99 (incl. cold-open), throughput de ingest,
  tamanho de arquivo e RSS de pico não regridem além do limiar; números publicados
  seguem [BENCHMARKS.md](BENCHMARKS.md) — incluindo onde perdemos para
  sqlite-vec/zvec.
- **Verificação:** `cargo bench` / `benches/run_all.sh`.

---

## Requisitos não-funcionais consolidados

| Requisito | Alvo | Verificação |
|---|---|---|
| Durabilidade | zero perda confirmada sob kill -9/queda de energia | crash harness (S4) |
| Integridade | arquivo nunca irrecuperável; recovery automático | crash harness + fuzzing |
| Latência `recall` | < 50 ms p99 @ 100k memórias, CPU-only | benchmark harness |
| Latência `remember` | < 200 ms p99 (dominada pelo embedding) | benchmark harness |
| Artefato de release | < 40 MB comprimido, incluindo modelo (ADR 0010) | CI de release |
| RAM | < 300 MB @ 100k memórias | benchmark harness (RSS) |
| Plataformas | Windows, Linux, macOS — mesmo arquivo, little-endian | CI matriz 3 SOs |
| Rede | ZERO chamadas no núcleo (auditável) | revisão + ausência de deps de rede |
| Robustez de código | `unsafe_code = forbid`; `unwrap/expect/panic = deny` na engine | lints de workspace no CI |
