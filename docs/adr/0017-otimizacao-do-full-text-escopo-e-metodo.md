# ADR 0017 — Otimização do full-text: escopo e método (profiling antes de estrutura)

**Status:** Aceito (jul/2026); **fase FT fechada nos números em 2026-07-13, NFR de latência
segue reprovado** (ver "Fechamento da fase FT" abaixo — decisão de prosseguir vs. aceitar a
limitação é do founder, pendente). Abre a fase FT (`03-tasks.md`), motivada pelo NFR
reprovado da story S16/BQ1 ([ADR 0015](0015-ef-search-default-escalado-pelo-indice.md)):
`recall` p99 @ 100k medido em 1.224,62 ms contra o teto de 50 ms — 24x acima.

## Contexto

A task BQ1 (`ef_search` escalonado) isolou a causa: a busca vetorial pura
(`Store::recall_vector`) mede **19,32 ms** de p99 @ 100k — dentro do NFR com
folga —, enquanto o `recall` híbrido completo mede **1.224,62 ms** no mesmo
run. A comparação lado a lado (medida com a métrica de
`query_vector_p50/p99_ms` introduzida no PR #9, comparável a
`query_engine_p50/p99_ms` — ambas excluem o tempo de embed):

| dataset | engine (híbrido, sem embed) p99 | vector-only p99 | razão |
|---|---:|---:|---:|
| agent-mem-10k | ~115 ms | ~10 ms | ~11x |
| agent-mem-100k | 1.215,34 ms | 19,32 ms | ~63x |

A degradação **não é do HNSW** (O(log N) por construção, comportamento
observado condiz) — é do meio full-text da fusão híbrida. Leitura do código
(`crates/embedmind-core/src/index/fts.rs::search`) mostra o mecanismo: para
cada termo da query, o BM25 varre **a lista de postings inteira daquele
termo** (`for p in &postings.entries`, sem corte antecipado, sem estrutura de
salto) — custo O(tamanho da postings list), que cresce linear com o corpus. A
10k o custo é imperceptível; a 100k, dominante.

O ADR 0011 (decisão original do full-text) rejeitou embutir `tantivy`
precisamente porque um motor de busca de terceiros escreve fora do arquivo
único e tem durabilidade própria fora do WAL — isso continua valendo. **Esta
fase não reabre essa decisão**: o full-text continua sendo um índice
invertido próprio, persistido nas páginas do `.mind`, integrado ao WAL. O que
esta fase resolve é a *forma de scan* dentro dessa estrutura, que o ADR 0011
nunca precisou especificar em detalhe (o `remember` que grava postings e o
`recall` que as varre são preocupações distintas).

## Decisão

### 1. Profiling antes de qualquer otimização estrutural

Nenhuma task de otimização entra em execução antes de uma task de
**profiling dedicado** produzir evidência de onde o tempo do `recall` híbrido
@ 100k é gasto dentro do meio full-text. Hipóteses candidatas, nenhuma
assumida a priori:

- Decodificação de postings (bytes → `Vec<Entry>`, alocação por termo).
- I/O de página (cache miss, leitura de disco da cadeia `FTS_POSTINGS`).
- Custo do `HashMap<Ulid, f32>` de scores (hashing de ULID por candidato).
- A closure `keep`/`doc_len` recarregando o registro inteiro por candidato
  para re-tokenizar (decisão do ADR 0011: `doc_len` não é persistido).

**Por quê:** otimizar sem medir arrisca gastar a fase inteira na causa
errada — cada hipótese acima tem uma correção diferente e nenhuma foi
descartada por medição, só por leitura de código. Decisão do founder
(2026-07-11): profiling primeiro é inegociável para esta fase.

### 2. Bump de `format_version` liberado quando a otimização exigir

`FORMAT_VERSION` está em 3 hoje. As otimizações estruturais reais de scan
(compressão delta/varint dos `record_id` nas postings, skip lists dentro de
uma postings list grande) mudam a codificação em disco das páginas
`FTS_POSTINGS`/`FTS_DICT` — exigem bump aditivo, no mesmo padrão que o ADR
0011 já previu ("um arquivo v1 não tem índice full-text... bump aditivo não
quebra: nenhum byte pré-existente muda de significado"). Decisão do founder
(2026-07-11): aceitar o bump quando a otimização exigir, **desde que**
aditivo — um arquivo `.mind` de `format_version` anterior continua legível
(degrada, na pior hipótese, para o scan antigo sobre postings não
comprimidas/sem skip list; nunca para erro).

### 3. Ordem de risco crescente

Depois do profiling apontar a causa dominante, as otimizações candidatas
entram na ordem que preserva o formato por mais tempo:

1. **Early termination / top-k sem materializar a lista inteira** — mudança
   de algoritmo de scan (loop `for p in &postings.entries` vira uma varredura
   que para cedo dado um limiar), sem tocar o formato em disco.
2. **Compressão delta+varint dos `record_id`** — reduz bytes decodificados
   por página; muda a codificação de `FTS_POSTINGS`, exige bump.
3. **Skip lists dentro de uma postings list grande** — o corte assintótico de
   verdade (pular blocos sem decodificar); estrutura de página nova, bump
   maior.
4. **Cache/pré-computação de `doc_len` ou IDF** — só entra se o profiling
   apontar a re-tokenização como custo relevante; reabre o trade-off que o
   ADR 0011 descartou deliberadamente ("um dado a menos que pode divergir em
   disco"), então exige justificativa própria, não é default.

Cada otimização é avaliada e aceita/rejeitada **pelo dado que o profiling e o
harness produzem**, não pela ordem — a lista acima é a sequência de
investigação, não um compromisso de implementar todas as quatro.

### 4. Critério de saída da fase

A fase fecha quando o NFR `recall p99 @ 100k < 50 ms` passa medido pelo
harness (`benches/run_all.sh --full`), **ou** quando o founder decide
conscientemente aceitar uma limitação de escala documentada (não é decisão
default desta fase — ver "Alternativas rejeitadas").

## Resultado do profiling (FT1/S24)

Medição feita com instrumentação manual (`Instant` por fase, `Store::search_text_profiled` +
binário `profile_fts`) sobre `agent-mem-100k` aquecido (metodologia BENCHMARKS.md §3), 1000
queries, 50 de warm-up. Relatório bruto completo em
[`benches/results/profile-fts-100k.txt`](../../benches/results/profile-fts-100k.txt).

Wall time medido pela instrumentação: p50 994,33 ms, p99 4.576,46 ms — consistente com a ordem
de grandeza do `1.224,62 ms` de p99 do harness citado no Contexto (mesma fase híbrida, método de
medição diferente).

| fase | total ms (1000 queries) | fração do tempo medido |
|---|---:|---:|
| postings lookup (I/O de página + decodificação) | 13.863,27 | 1,2% |
| **keep (recarga do registro + re-checagem de tombstone/scope/filtro)** | **1.030.310,79** | **88,8%** |
| doc_len (recarga do registro + re-tokenização) | 52.272,67 | 4,5% |
| scoring (acumulação no `HashMap` + sort) | 63.436,74 | 5,5% |

18.090 termos casados no total; 366.468.601 postings visitados no total (~366.469 por query em
média).

**Causa dominante identificada:** a closure `keep` — não a decodificação de postings, não o
hashing do `HashMap<Ulid, f32>`, não a re-tokenização de `doc_len` isolada. `keep` sozinha
responde por 88,8% do tempo do meio full-text, acima do limiar de 60% que definia "causa
dominante" no critério de pronto desta task. As hipóteses de I/O de página e custo do `HashMap`
ficam descartadas por medição (1,2% e 5,5% respectivamente, não 4 — o `HashMap` está embutido em
"scoring" no instrumentado, não isolado à parte, mas de qualquer forma marginal frente a `keep`).

Candidato natural para a próxima task de otimização (FT2): eliminar ou reduzir a recarga do
registro inteiro dentro de `keep` por candidato — não decidido aqui, apenas anotado; esta task é
somente medição, nenhuma otimização entra neste commit.

## Fechamento da fase FT — números finais @ 10k e @ 100k

Medição oficial `benches/run_all.sh --full` (1000 queries, ambos os datasets, `agent-mem-10k`
regenerado e `agent-mem-100k` regenerado pelo founder em `format_version` 4 — header `MINDFMT1 04
…` confirmado byte a byte), rodada de 2026-07-13, publicada em
[`benches/results/0.1.0-dev.json`](../../benches/results/0.1.0-dev.json) (espelho legível em
`latest.md`, mesma invocação). Compreende o efeito acumulado de FT2 (early termination, ADR 0018)
+ FT3 parte 1 (delta+varint, ADR 0021) sobre o baseline original desta ADR. **Não** inclui o efeito
de FT3 parte 2 (skip lists, `format_version` 5, ADR 0022): a estrutura de skip foi implementada e
testada, mas o `.mind` desta rodada é v4 (delta+varint sem skip) e, mesmo num arquivo v5, o hot path
de `fts::search` ainda materializa a lista inteira de cada termo na passada de bounds — o skip index
só corta trabalho de verdade com uma reescrita dessa passada em BlockMax-WAND, deliberadamente fora
do escopo do ADR 0022 (ver esse ADR §5, "Honestidade sobre onde o ganho entra").

### `recall` p99 — end-to-end (embed + engine híbrido)

| dataset | antes (baseline desta ADR / FT5 confirm) | depois (FT2+FT3-parte-1, esta rodada) | razão |
|---|---:|---:|---:|
| agent-mem-10k | ~115 ms (§Contexto, pré-FT) | **30,15 ms** | ~3,8x |
| agent-mem-100k | 1.224,62 ms (§Contexto) / 956,80 ms (confirmação oficial FT5, `docs/adr/0020`) | **224,88 ms** | ~5,5x vs. FT5, ~5,4x vs. baseline original |

Decomposição @ 100k desta rodada (`query_embed_p99_ms` / `query_engine_p99_ms` / `query_vector_p99_ms`
do JSON): embed 6,21 ms · engine (FTS+fusão+load, sem embed) 219,55 ms · vetor puro (HNSW só) 29,32
ms. Os ~190 ms de diferença entre engine e vetor-only continuam sendo o meio full-text — o mesmo
gargalo isolado na FT1, reduzido de ordem de grandeza mas não eliminado.

@ 10k a mesma decomposição: embed 5,52 ms · engine 25,29 ms · vetor puro 8,26 ms.

### recall@10 (tie-aware, ADR 0019) — média / p10 / p50 / mín

| dataset | média | p10 | p50 | mín |
|---|---:|---:|---:|---:|
| agent-mem-10k | 1,0000 | 1,0000 | 1,0000 | 1,0000 |
| agent-mem-100k | 1,0000 | 1,0000 | 1,0000 | 1,0000 |

Sem regressão em nenhum dataset desde a S27 (tie-aware grading, ADR 0019) — os números eram já
1,0000/1,0000 antes desta fase de otimização de scan; FT2/FT3 não tocam BM25/HNSW/RRF, então essa
paridade era esperada, não uma surpresa desta rodada.

### RSS de pico — ingest / query

| dataset | ingest | query |
|---|---:|---:|
| agent-mem-10k | 96,60 MiB | 99,24 MiB |
| agent-mem-100k | 117,01 MiB | 118,25 MiB |

Consistente com o fechamento da FT5 (ADR 0020, ~120 MiB nessa mesma medição em 2026-07-12) — bem
dentro do teto de 300 MiB, nenhuma regressão introduzida pela FT3.

### Veredito dos NFRs desta fase

| NFR | alvo | medido @ 100k | veredito |
|---|---|---:|:---:|
| `recall` p99 (end-to-end) | < 50 ms | 224,88 ms | ❌ **reprovado** |
| pior query (recall@10, tie-aware) | ≥ 0,70 | 1,0000 (mín) | ✅ aprovado |
| RSS de pico | < 300 MiB | 118,3 MiB (query) / 117,0 MiB (ingest) | ✅ aprovado |

**O NFR de latência segue reprovado, registrado sem meias-palavras.** A fase FT reduziu o p99 do
`recall` híbrido @ 100k em ~5,4x (1.224,62 ms → 224,88 ms) através de três mudanças que preservam
byte-a-byte a equivalência de resultado (FT2 early termination, FT3 delta+varint, FT3 skip-index
estrutural) — mas o teto de 50 ms definido no NFR original não foi alcançado. O caminho conhecido e
já projetado para o próximo corte (ligar o skip index de fv5 ao hot path via BlockMax-WAND, ADR
0022 §5) não foi executado nesta fase porque muda a ordem de avaliação dos candidatos e é
equivalence-risky o bastante para exigir sua própria task, com o dado desta medição em mãos.

**Decisão pendente do founder** (não tomada nesta sessão, apenas documentation-only): prosseguir com
uma quinta task (BlockMax-WAND sobre o skip index fv5, mirando fechar os ~190 ms restantes do meio
full-text) ou aceitar 224,88 ms como limitação de escala documentada para o lançamento do M1,
revisitando pós-tração. As duas opções descritas no ADR original (§"Critério de saída da fase") —
"passa medido" ou "founder decide conscientemente aceitar uma limitação documentada" — continuam
ambas em aberto; esta sessão só fecha a contabilidade de números, não escolhe entre elas.

## Alternativas rejeitadas

- **Modo `vector_only` opcional exposto ao usuário, sem otimizar o FTS**:
  cogitado e descartado. O diferencial de posicionamento do produto
  (`00-prd.md` §3: "híbrido de verdade... nenhum embarcado tem o trio
  completo") depende do full-text estar disponível por padrão; empurrar a
  escolha para o usuário dilui esse diferencial e não resolve o NFR, só o
  contorna. Fica registrado como fallback se o profiling mostrar que a causa
  dominante não é corrigível dentro do prazo do launch — não como plano A.
- **Substituir por `tantivy` ou outro motor externo**: já rejeitado pelo ADR
  0011 pelos mesmos motivos (quebra "um arquivo", quebra o WAL único); nada
  mudou desde então que reabra essa decisão.
- **Aceitar a limitação de escala sem investigar**: rejeitado pelo founder —
  a diferença de 63x entre vetor-only e híbrido a 100k é grande demais para
  descartar sem profiling; pode ser um bug barato de corrigir (ex.: uma
  alocação óbvia), não necessariamente um limite estrutural genuíno.

## Consequências

- A fase FT não é uma task, é uma sequência: profiling → decisão de causa →
  otimização(ões) na ordem de risco → validação do NFR pelo harness. Tasks
  subsequentes (`03-tasks.md`) dependem do resultado da anterior — a
  derivação registra isso como pré-requisito de leitura, não como grafo
  formal (mesmo padrão de "DEPENDÊNCIA AUSENTE" já usado no projeto).
- Se o profiling apontar mais de uma causa relevante, cada uma vira sua
  própria story/task — não empacotar num "otimizar tudo de uma vez" que
  dificulta validar o que efetivamente comprou o ganho.
- Falhas de dataset pré-existentes (o `.mind` de `format_version` 1/2 sem
  índice full-text) continuam degradando para vetor-only com aviso — nenhuma
  otimização desta fase pode remover essa rota de degradação existente
  (ADR 0011).
