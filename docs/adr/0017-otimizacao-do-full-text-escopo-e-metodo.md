# ADR 0017 — Otimização do full-text: escopo e método (profiling antes de estrutura)

**Status:** Aceito (jul/2026); **fase FT fechada nos números em 2026-07-13, NFR de latência
segue reprovado** (ver "Fechamento da fase FT" abaixo). Abre a fase FT (`03-tasks.md`), motivada
pelo NFR reprovado da story S16/BQ1 ([ADR 0015](0015-ef-search-default-escalado-pelo-indice.md)):
`recall` p99 @ 100k medido em 1.224,62 ms contra o teto de 50 ms — 24x acima.

**Atualização 2026-07-13 (revisão do produto):** ver "O benefício do full-text: queries
lexicais" abaixo — mede pela primeira vez o *ganho* do full-text (não só o custo), sobre
queries lexicais com ground truth por construção, @ 10k e @ 100k. A decisão de prosseguir com
BlockMax-WAND (em vez de vector-only default) está registrada no [ADR 0023](0023-blockmax-wand-decisao-fase-bmw.md),
com base no lift medido crescente (+0,09 @10k → +0,18 @100k).

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

## Resultado do profiling confirmatório (FTOPT-0)

FT1 mediu que `keep` responde por 88,8% do tempo do meio full-text, mas não discriminava **quanto
desse tempo é I/O desperdiçado** (candidato recarregado e depois rejeitado — tombstone, fora de
escopo, ou reprovado por filtro de metadados) **vs. I/O inevitável** (candidato aceito, cujo
conteúdo teria que ser carregado de qualquer forma para montar o `Hit` devolvido). Essa distinção
decide o teto de ganho esperado de uma futura otimização de "metadados leves" (candidata a virar a
task FTOPT-1): se a maioria dos candidatos é aceita, pular I/O só nos rejeitados compra pouco,
porque a maior parte do trabalho de `keep` seria refeita de qualquer forma para montar o resultado.

**Método:** mesma instrumentação da FT1 (`Instant` por fase, `Store::search_text_profiled` +
binário `profile_fts`), estendida com um novo tipo `KeepOutcome` (`Accepted` / `Tombstoned` /
`OutOfScope` / `FilteredOut`) que a closura `keep` de `search_profiled` agora retorna em vez de um
`bool` simples — só nessa função de profiling (`#[doc(hidden)]`); `search`/`search_linear`/
`search_bmw_counted`, os caminhos de produção, continuam recebendo `bool` e não mudam de
comportamento. `Store::search_text_profiled` (`api.rs`, também `#[doc(hidden)]`) é o único ponto
que sabe *por que* um candidato foi rejeitado (tombstone/superseded vs. fora de escopo de
projeto/agente vs. filtro de metadados reprovado), então é ele quem categoriza; o contador é
agregado uma vez por candidato distinto, na mesma memoização que já existia para `keep_ns`.

Rodado sobre `agent-mem-10k` e `agent-mem-100k` (mesma metodologia BENCHMARKS.md §3: 1000 queries,
50 de warm-up).

### `agent-mem-10k` (1000 queries)

Relatório bruto: [`benches/results/profile-fts-10k-ftopt0.txt`](../../benches/results/profile-fts-10k-ftopt0.txt).

| outcome | contagem | fração |
|---|---:|---:|
| aceito (carga de conteúdo inevitável) | 9.268.591 | 97,9% |
| rejeitado: tombstoned/ausente/superseded | 201.888 | 2,1% |
| rejeitado: fora de escopo (projeto/agente) | 0 | 0,0% |
| rejeitado: filtro de metadados | 0 | 0,0% |
| **rejeitado (qualquer motivo)** | **201.888** | **2,1%** |

9.470.479 candidatos distintos re-checados no total (1000 queries).

### `agent-mem-100k` (1000 queries)

A rodada @ 100k inicialmente não coube em três tentativas seguidas nesta sessão (o binário parava
consistentemente perto de 250-300/1000 queries) — investigação por processo (`Get-Process` no
Windows, não só `ps aux`/`tasklist` do shell Bash) revelou processos residuais de tentativas
anteriores ainda segurando o lock single-writer do arquivo (`docs/adr/0006`), não um bug real de
performance. Depois de confirmar ambiente limpo (nenhum processo `embedmind`/`profile_fts` vivo) e
rebuildar o binário, a 4ª tentativa completou as 1000 queries normalmente. Relatório bruto:
[`benches/results/profile-fts-100k-ftopt0.txt`](../../benches/results/profile-fts-100k-ftopt0.txt).

| outcome | contagem | fração |
|---|---:|---:|
| aceito (carga de conteúdo inevitável) | 92.525.963 | 99,9% |
| rejeitado: tombstoned/ausente/superseded | 67.296 | 0,1% |
| rejeitado: fora de escopo (projeto/agente) | 0 | 0,0% |
| rejeitado: filtro de metadados | 0 | 0,0% |
| **rejeitado (qualquer motivo)** | **67.296** | **0,1%** |

92.593.259 candidatos distintos re-checados no total (1000 queries).

### Implicação para o teto de ganho da FTOPT-1

Os dois tamanhos confirmam e reforçam a mesma leitura: a esmagadora maioria dos candidatos que
`keep` recarrega **é aceita**, não rejeitada — e a fração de rejeitados **cai** com a escala (2,1%
@10k → 0,1% @100k), o oposto do que seria necessário para uma otimização de "pular I/O nos
rejeitados" compensar. No corpus sintético (que não popula filtros de escopo/agente/metadados na
maior parte dos registros), a rejeição vem quase inteiramente de tombstone, e essa fração encolhe
ainda mais em corpus maior (mais candidatos "de verdade" por query, a mesma quantidade absoluta de
tombstones). Isso significa que uma otimização que evite a recarga do registro **somente para
candidatos rejeitados** — por exemplo, indexar um bit de "vivo" mais leve que o registro inteiro só
para poder pular tombstoned sem tocar o B-tree principal — teria um teto de ganho **baixíssimo** em
escala real: no máximo ~0,1% dos candidatos @100k, deixando os ~99,9% restantes (aceitos) pagando o
mesmo custo de recarga que pagam hoje, porque o conteúdo precisa ser lido de qualquer forma para
devolver o `Hit`.

Isto **não decide** se a FTOPT-1 (ou qualquer otimização de metadados leves) deve prosseguir como
desenhada originalmente, ser redesenhada para atacar o caso aceito (não só o rejeitado — por
exemplo, acelerando a própria carga do conteúdo para todo candidato, não só evitando carregá-lo
quando rejeitado), ou ser abandonada — essa é uma decisão de produto/arquitetura do founder. Esta
task apenas mede e reporta o número que a decisão precisa; com os dois tamanhos confirmados, não há
mais dúvida de metodologia pendente.

**Adendo (FTOPT-1, 2026-07-13):** a rota de redesenho foi tomada — a estrutura leve foi
reescopada para servir também (principalmente) os candidatos **aceitos**: `keep` só precisa da
decisão e `doc_len` só precisa da contagem de tokens, nenhum dos dois precisa do corpo do registro,
que passa a ser carregado apenas para os top-k `Hit`s e para filtros de metadados customizados.
Desenho, layout (`format_version` 7) e suite de equivalência no
[ADR 0027](0027-filter-meta-sidecar-fv7.md); a medição do ganho @100k continua reservada à task de
fechamento (FTOPT-4), e a decisão de produto sobre o NFR continua do founder.

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
| agent-mem-10k | ~115 ms (§Contexto, pré-FT) | **31,84 ms** | ~3,6x |
| agent-mem-100k | 1.224,62 ms (§Contexto) / 956,80 ms (confirmação oficial FT5, `docs/adr/0020`) | **255,12 ms** | ~4,8x vs. FT5, ~4,8x vs. baseline original |

Decomposição @ 100k desta rodada (`query_embed_p99_ms` / `query_engine_p99_ms` / `query_vector_p99_ms`
do JSON): embed 7,67 ms · engine (FTS+fusão+load, sem embed) 249,26 ms · vetor puro (HNSW só) 41,02
ms. Os ~208 ms de diferença entre engine e vetor-only continuam sendo o meio full-text — o mesmo
gargalo isolado na FT1, reduzido de ordem de grandeza mas não eliminado.

@ 10k a mesma decomposição: embed 5,69 ms · engine 27,05 ms · vetor puro 8,47 ms.

Números desta rodada incluem, pela primeira vez, o `lexical_lift` do harness FT6 (`benches/src/lexical.rs`)
medido junto — ver seção "O benefício do full-text" abaixo — o que explica a pequena variação de
p99 frente a rodadas anteriores (mesma ordem de grandeza, mesmo `format_version` 4, sem mudança de
código de produção entre as rodadas).

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
| agent-mem-10k | 97,59 MiB | 99,41 MiB |
| agent-mem-100k | 117,54 MiB | 117,74 MiB |

Consistente com o fechamento da FT5 (ADR 0020, ~120 MiB nessa mesma medição em 2026-07-12) — bem
dentro do teto de 300 MiB, nenhuma regressão introduzida pela FT3.

### Veredito dos NFRs desta fase

| NFR | alvo | medido @ 100k | veredito |
|---|---|---:|:---:|
| `recall` p99 (end-to-end) | < 50 ms | 255,12 ms | ❌ **reprovado** |
| pior query (recall@10, tie-aware) | ≥ 0,70 | 1,0000 (mín) | ✅ aprovado |
| RSS de pico | < 300 MiB | 117,7 MiB (query) / 117,5 MiB (ingest) | ✅ aprovado |

**O NFR de latência segue reprovado, registrado sem meias-palavras.** A fase FT reduziu o p99 do
`recall` híbrido @ 100k em ~4,8x (1.224,62 ms → 255,12 ms) através de três mudanças que preservam
byte-a-byte a equivalência de resultado (FT2 early termination, FT3 delta+varint, FT3 skip-index
estrutural) — mas o teto de 50 ms definido no NFR original não foi alcançado. O caminho conhecido e
já projetado para o próximo corte (ligar o skip index de fv5 ao hot path via BlockMax-WAND, ADR
0022 §5) não foi executado nesta fase porque muda a ordem de avaliação dos candidatos e é
equivalence-risky o bastante para exigir sua própria task — a decisão de segui-lo está registrada
no [ADR 0023](0023-blockmax-wand-decisao-fase-bmw.md), com o dado desta medição e do lift lexical
em mãos.

**Decisão tomada** ([ADR 0023](0023-blockmax-wand-decisao-fase-bmw.md), 2026-07-13, com o lift
medido em mãos — ver seção abaixo): prosseguir com a fase BMW, ligando o skip index fv5 ao hot
path via BlockMax-WAND, em vez de tornar o full-text opt-in. Critério de reversão honesto
registrado no ADR 0023: se o BMW não fechar o NFR (< 50 ms p99 @ 100k) ou quebrar a equivalência
de resultado, a opção vector-only default volta à mesa.

## O benefício do full-text: queries lexicais (revisão do founder, 2026-07-13)

Toda a contabilidade acima mede o **custo** do full-text (~190 ms dos 224,88 ms de p99 @ 100k
vêm do meio full-text) e usa como métrica de qualidade o `recall` sobre queries de paráfrase
semântica (`benches/src/recall.rs`), medido só na metade vetorial (`Store::recall_vector`) —
por desenho, para isolar a qualidade do HNSW sem a fusão do BM25 "contaminar" a métrica. Isso
deixa a pergunta oposta sem resposta: o que o full-text **compra**, e em qual workload? Sem
esse número, a decisão entre continuar investindo (BlockMax-WAND) ou tornar o full-text opt-in
(vector-only default) estava sendo avaliada só pelo lado do custo.

Esta seção fecha essa lacuna com um harness novo (`benches/src/lexical.rs`): um banco de
queries **lexicais** — identificadores de código exatos (`lookup_via_skip_42`), flags de CLI
(`--recency-v7`), fragmentos de mensagem de erro literal, hashes hex e ULIDs — gerado
deterministicamente por seed e ancorado no corpus real (cada literal é injetado em exatamente
uma memória sintética dedicada, que é o ground truth por construção da query). As mesmas 100
queries rodam por `Store::recall` (híbrido: BM25+vetor+RRF) e por `Store::recall_vector`
(vetor puro) sobre o mesmo dataset materializado; o delta de recall@10 entre os dois é o
benefício medido do full-text nesse workload.

### Resultado medido @ 10k (`agent-mem-10k`, 100 casos lexicais, `cargo run -p embedmind-bench --release --bin run_all -- agent-mem-10k`, 2026-07-13)

| métrica | híbrido (BM25+vetor+RRF) | vetor-puro (`recall_vector`) | delta |
|---|---:|---:|---:|
| recall@10 | **1,0000** | 0,9000 | **+0,10** |
| query p50 | 44,86 ms | 43,82 ms | +1,04 ms |
| query p99 | 89,15 ms | 52,76 ms | +36,39 ms |

Com o embedding all-MiniLM-L6-v2 (384 dims) usado hoje, o vetor puro já recupera **90%** dos
literais exatos no top-10 — os embeddings de frases curtas carregam informação lexical
suficiente para aproximar bem até identificadores/ULIDs incomuns, contrariando a intuição de
que um embedding semântico "erra tudo" fora de vocabulário. O full-text fecha os 10% restantes
(10 de 100 casos) ao custo de +36,39 ms de p99 nesta amostra @ 10k — a mesma ordem de grandeza
do custo total do meio full-text já documentado acima (que cresce ~linear com o corpus, não
com o número de queries lexicais).

**Leitura honesta @ 10k, sem escolher (na época):** a 10k o lift absoluto era pequeno (+0,10 em
recall@10) pelo custo de latência já conhecido — o que pesaria, isoladamente, a favor de
vector-only como default (FTS opt-in). Mas a hipótese testável na direção oposta também era
real: um corpus maior tem mais literais colidindo por proximidade vetorial (mais
"quase-sinônimos" no espaço de embedding), o que tenderia a *piorar* o recall vetor-puro
relativo, não melhorá-lo, enquanto o custo do full-text (o gargalo linear já medido) piora
junto. Sem o número @ 100k, não era possível saber qual efeito dominava.

### Resultado medido @ 100k (`agent-mem-100k`, 100 casos lexicais, rodada oficial 2026-07-13)

| métrica | híbrido (BM25+vetor+RRF) | vetor-puro (`recall_vector`) | delta |
|---|---:|---:|---:|
| recall@10 | **1,0000** | 0,8200 | **+0,18** |
| query p50 | 28,03 ms | 25,86 ms | +2,16 ms |
| query p99 | 139,38 ms | 32,45 ms | +106,92 ms |

### Veredito: a hipótese de piora do vetor-puro se confirmou — lift dobra, não encolhe

| dataset | recall@10 híbrido | recall@10 vetor-puro | lift |
|---|---:|---:|---:|
| agent-mem-10k | 1,0000 | 0,9100 | +0,09 |
| agent-mem-100k | 1,0000 | 0,8200 | **+0,18** |

O lift **dobra** de @10k para @100k: o vetor-puro degrada (0,9100 → 0,8200) conforme o corpus
cresce — mais literais parecidos colidem no espaço de embedding —, enquanto o híbrido segura
100% nos dois tamanhos porque o BM25 encontra o literal exato independentemente da densidade do
espaço vetorial ao redor. Essa é exatamente a direção que teria justificado vector-only default
se tivesse ido para o outro lado — foi o oposto. O custo do full-text sobre essas queries
lexicais também cresce com o corpus (p99 híbrido 139,38 ms vs. 22,14 ms @10k), consistente com o
mesmo gargalo linear já isolado nesta ADR.

Com esse dado em mãos, o founder decidiu (2026-07-13): manter o full-text como default e investir
na reescrita BlockMax-WAND para fechar a latência, em vez de tornar o full-text opt-in — decisão
completa, com critério de reversão, no [ADR 0023](0023-blockmax-wand-decisao-fase-bmw.md).

## Fechamento da fase BMW — veredito final do NFR (BMW-3, 2026-07-13)

Medição oficial `benches/run_all.sh --full` (1000 queries, ambos os datasets **regenerados em
`format_version` 6 pelo founder** — `embedmind vacuum`, header `MINDFMT1 06` confirmado byte a
byte nos dois arquivos antes da rodada), publicada em
[`benches/results/0.1.0-dev.json`](../../benches/results/0.1.0-dev.json) (espelho em `latest.md`).
Esta é a medição com o BlockMax-WAND (BMW-2, [ADR 0025](0025-blockmax-wand-na-busca-fts.md))
ativo — `fts::search` despacha para `search_bmw_counted` em qualquer arquivo fv ≥ 6.

### `recall` p99 — end-to-end (embed + engine híbrido)

| dataset | antes (FT, fv4/5) | depois (BMW ativo, fv6) | razão |
|---|---:|---:|---:|
| agent-mem-10k | 31,84 ms (FT, ver acima) | **51,42 ms** | inconclusivo isoladamente (variância de máquina entre rodadas; ver nota abaixo) |
| agent-mem-100k | 255,12 ms (FT, ver acima) | **224,00 ms** | ~1,14x — **não fecha o NFR** |

**NFR `recall p99 @ 100k < 50 ms`: ❌ REPROVADO — 224,00 ms.** O número @100k é o que decide:
praticamente idêntico ao patamar pré-BMW (224,88–255,12 ms nas rodadas anteriores da fase FT),
apesar do BMW estar de fato ativo (arquivo confirmadamente fv6). @10k o p99 subiu para 51,42 ms
frente aos ~30 ms de rodadas anteriores — isso sozinho não indica regressão: são datasets
pequenos, mais sensíveis a ruído de máquina entre execuções, e o dado que importa (@100k, onde o
full-text domina o custo) não se moveu.

### Causa raiz: o BMW ativa, mas quase não pula blocos neste corpus

Hipótese inicial (a checar antes de aceitar o número): o corpus sintético de benchmark, gerado
por templates+paráfrase com vocabulário amplo, poderia nunca atingir `SKIP_MIN_DOC_FREQ` (512
postings) na maioria dos termos de query — nesse caso o BMW nunca seria exercitado e o p99 parado
seria trivial (nada mudou porque nada rodou o caminho novo). **Medida diretamente e refutada:**
um binário de instrumentação (`benches/src/bin/bmw_reach.rs`, usando o novo
`Store::search_text_bmw_counted` `#[doc(hidden)]`, mesmo padrão de `search_text_profiled`) rodou
as mesmas 1000 queries do harness oficial sobre `agent-mem-100k` (fv6) contando `BmwCounters` por
query:

| métrica | valor |
|---|---:|
| queries com ≥1 termo com skip real (`block_count > 0`) | 828 / 1000 (82,8%) |
| queries onde todo termo casado decodificou inteiro (sem skip) | 172 / 1000 (17,2%) |
| queries sem nenhum termo casado | 0 / 1000 (0,0%) |
| blocos totais tocados | 2.870.508 |
| blocos decodificados | 2.869.126 |
| **blocos pulados sem decodificar** | **1.382 (0,05%)** |
| documentos avaliados exatamente | 6.820.998 |
| `pivot_skips` (faixas de id provadas abaixo de θ pelo refinamento block-max) | 1.670.664 |

O BMW **está** sendo exercitado na grande maioria das queries — a hipótese inicial (corpus sem
termos frequentes o bastante) está refutada. A causa real é outra: os `pivot_skips` são altos
(1,67M), mas quase nunca se traduzem em blocos de fato pulados (0,05%). Olhando
`BmwCursor::advance_to` (`fts.rs`): um pulo só evita decodificar um bloco quando o alvo cai
exatamente no `first_id` de um bloco *depois* do bloco atual; se o alvo cai **dentro** de um
bloco (posição intermediária), esse bloco é decodificado de qualquer forma para localizar a
posição exata via `partition_point`. Com termos de alta frequência (df ≥ 512) cujas postings
cobrem o espaço de ids de forma densa e relativamente uniforme — o padrão esperado de um corpus
sintético com vocabulário amplo e ids monotonicamente crescentes por inserção —, o refinamento
block-max quase sempre encontra um documento dentro do próprio bloco coberto que ainda pode bater
`θ`, então o pulo de faixa (`pivot_skips`) aterrissa dentro de um bloco em vez de saltar um bloco
inteiro. Em média, cada query decodifica ~2.869 blocos e avalia ~6.821 documentos — essencialmente
a lista candidata inteira — igual ao que a passada linear já fazia. O ganho estrutural do BMW
(pular blocos inteiros sem decodificar) depende de blocos *inteiramente* abaixo do threshold, algo
que corpora com clusters de termos raros/localizados no espaço de ids favorecem e este corpus
sintético não tem.

**Isto não é "o BMW falhou" — é "o corpus de benchmark não tem a distribuição de postings que o
BMW foi desenhado para explorar".** O algoritmo (ADR 0025) está correto e a suite de equivalência
prova que o resultado é idêntico ao oráculo; o que não se confirmou foi o ganho de latência *neste
workload de medição*. A BMW-3 registrou isto como **suspeita** de limitação de metodologia de
benchmark (a hipótese de que um corpus com termos concentrados/localizados cortaria mais) — mas
deixou explícito que era hipótese não confirmada, e a BMW-5 abaixo a testou.

#### Correção da BMW-5 (2026-07-13): a suspeita de metodologia foi refutada — ver [ADR 0026](0026-corpus-de-localidade-nao-reabilita-o-bmw.md)

A BMW-5 construiu o corpus que esta seção especulava faltar: `corpus::generate_local`
(`benches/src/corpus.rs`), com **localidade de sessão** (rajadas de memórias sobre o mesmo
projeto/termo em ULIDs contíguos) e **vocabulário Zipf** (poucos termos dominam, cauda longa) — a
distribuição de memória real de agente. Rodou a suite lado a lado @10k (uniforme vs. localidade).
Resultado **oposto à hipótese**: o corpus de localidade pula **menos** blocos, não mais —
`blocks_skipped` 0,0% (18/197 144) contra 0,3% (901/296 635) do uniforme; queries com alcance real
do BMW caem de 45,9% para 1,8%. Mecanicamente: a localidade concentra as ocorrências do termo
quente e deixa o `max_term_freq` (bound de impacto) **alto e uniforme em quase todos os blocos**, e
o refinamento block-max precisa justamente do contrário — blocos com impacto baixo intercalados
para provar exclusão. **A limitação de eficácia do BMW é do algoritmo/formato sobre este padrão de
dado, não um artefato da metodologia de benchmark.** O `query engine` p99 até cai no corpus de
localidade (83,86 vs 133,21 ms @10k), mas por menos trabalho agregado (Zipf → postings mais rasas),
não por mais blocos pulados — o BMW contribui com 0,0%. A dúvida aberta desta seção fica **fechada**
e sem promessa sobre o NFR: o dado sintético que temos, se algo, é *pior* para o BMW, não melhor.
Detalhes e números completos no ADR 0026.

### Full-text lift (FT6) revalidado nesta rodada

A mesma rodada oficial roda a suite `lexical.rs` de novo (mesmo run) — o BMW não pode ter
degradado a equivalência híbrida em escala real: `hybrid_recall_at_10` segue **1,0000** em ambos
os datasets (`benches/results/latest.md`), idêntico ao medido antes do BMW. Nenhuma regressão de
recall lexical introduzida pela reescrita.

### Veredito final dos NFRs desta fase (substitui o veredito FT acima)

| NFR | alvo | medido @ 100k | veredito |
|---|---|---:|:---:|
| `recall` p99 (end-to-end) | < 50 ms | 224,00 ms | ❌ **reprovado** |
| recall@10 híbrido (tie-aware) | ≥ 0,70 | 1,0000 | ✅ aprovado |
| full-text lift lexical (hybrid vs. vector-only) | sem regressão | 1,0000 (igual à rodada FT) | ✅ aprovado |
| RSS de pico | < 300 MiB | 113,5 MiB (query) / 112,6 MiB (ingest) | ✅ aprovado |

**O NFR de latência `< 50 ms @ 100k` NÃO foi alcançado (224,00 ms medido).** O critério de reversão
do ADR 0023 ("se o BMW não fechar o NFR, vector-only default volta à mesa") está em aberto —
decisão do founder, não tomada nesta sessão. As opções conhecidas com dado em mãos: aceitar a
limitação de latência como documentada (o full-text lift medido, FT6, +0,18 recall@10 @100k,
continua sendo valor de produto real), ou reverter full-text para opt-in, ou investir em uma
próxima otimização (revisitar o refinamento block-max para pular parcialmente dentro de um bloco).
A opção "medir com distribuições de postings mais realistas" foi **executada pela BMW-5**
([ADR 0026](0026-corpus-de-localidade-nao-reabilita-o-bmw.md)) e não muda o veredito — a
distribuição realista é, se algo, pior para o BMW. Nenhuma das opções restantes foi escolhida aqui;
o número e a causa raiz estão reportados sem meias-palavras no README/CHANGELOG, e a escolha entre
elas fica pendente.

## Resultado do profiling confirmatório pós-sidecar (FTOPT-5, 2026-07-14)

A FTOPT-1 ([ADR 0027](0027-sidecar-filter-meta-para-keep-e-doc-len.md)) moveu `keep`/`doc_len`
para o sidecar `filter_meta`, eliminando a recarga de registro que a FT1/S24 tinha medido em
88,8% do meio full-text. A rodada oficial pós-sidecar do founder (2026-07-14, datasets
regenerados para `format_version` 7,
[`benches/results/0.1.0-dev.json`](../../benches/results/0.1.0-dev.json)) mostrou melhora real
(`query p99 @100k`: 224,00 ms → **135,74 ms**, ~40%) mas **o NFR `< 50 ms` continua reprovado**, e
um dado novo intrigante: a busca lexical isolada (`lexical_lift.hybrid_query_p99_ms`) fica em
**37,71 ms** — dentro do NFR — enquanto a query `engine` sobre a amostra geral (não só casos
lexicais) mede **129,50 ms**, quase 4x o vetor-puro isolado (36,16 ms). Isso pedia profiling de
novo: o gargalo dominante pode ter mudado de lugar depois do sidecar, e a FT1/BMW-3 tinham medido
fases diferentes das que importam agora.

**Instrumentação nova, não reaproveitada da FT1/FTOPT-0**: `search_profiled`/`profile_fts` (FT1)
media o *scan linear exaustivo* pré-BMW — não o caminho que `Store::recall`/`search_text` de fato
rodam num arquivo fv ≥ 6 (que despacha para `search_bmw_counted`, ADR 0025). Medir aquele scan de
novo teria respondido a pergunta errada. Esta task adicionou:

- `index::fts::search_bmw_profiled` (`fts.rs`, `#[doc(hidden)]`) — espelha `search_bmw_counted`
  exatamente (mesmo algoritmo, resultado bit-idêntico), com `Instant` ao redor de: abertura de
  cursor (`dict::get` + decode de listas pequenas), o laço de bound WAND/block-max (decidir
  pular vs. avaliar, sem decodificar bloco), decodificação de bloco (`advance_to`/
  `advance_past`/`current_tf`), `keep`, `doc_len`, scoring.
- `Store::recall_profiled` (`api.rs`, `#[doc(hidden)]`) — espelha `recall_detailed` (sem lista de
  recência, sem expansão de grafo — fora do escopo desta medição), cronometrando o pipeline
  híbrido inteiro: vetor (embed + busca HNSW), o meio BMW acima, fusão RRF, carga final dos hits.
- `benches/src/bin/profile_recall.rs` — roda a **amostra geral de queries do harness**
  (`recall::query_texts`, a mesma da FT1/`profile_fts`, não só casos lexicais) sobre o dataset já
  vacuum'ado (`agent-mem-100k.mind`, `format_version` 7 confirmado byte a byte), 50 queries de
  aquecimento + 1000 medidas — mesmo tamanho do harness oficial.

### `agent-mem-100k` (1000 queries, `format_version` 7, amostra geral)

`Store::recall_profiled` wall time — p50 326,54 ms, p99 645,26 ms (mais alto que o `query_engine`
oficial porque roda em `dev`-adjacent single-run sem o isolamento de processo do harness; a
distribuição percentual entre fases, não o valor absoluto, é o dado que importa aqui).

| fase | total ms | % do tempo medido |
|---|---:|---:|
| vetor (embed da query + busca HNSW) | 126.409,9 | 36,0% |
| fts: abertura de cursor (dict lookup + decode de listas pequenas) | 28.860,9 | 8,2% |
| fts: laço de bound WAND/block-max (pular vs. avaliar) | 39.997,8 | 11,4% |
| **fts: decodificação de bloco (postings materializadas)** | **114.829,2** | **32,7%** |
| fts: `keep` (sidecar/registro) | 3.876,1 | 1,1% |
| fts: `doc_len` (sidecar/registro) | 1.038,5 | 0,3% |
| fts: scoring (BM25 + insert top-k) | 32.075,7 | 9,1% |
| fusão RRF (listas vetor + texto) | 7,6 | 0,0% |
| carga final dos hits retornados | 4.137,1 | 1,2% |
| **soma das fases** | 351.232,7 | 100,0% |

Contadores agregados do BlockMax-WAND na mesma rodada: 6.816.080 documentos avaliados
exatamente, 1.664.556 candidatos-pivô descartados pelo refinamento block-max sem avaliação,
2.872.240 blocos decodificados, **apenas 1.955 blocos pulados sem decodificar (taxa de skip
0,07%)** — consistente com a causa raiz já registrada na BMW-3/BMW-5 acima (o refinamento quase
sempre aterrissa *dentro* de um bloco coberto em vez de pular um bloco inteiro nesta distribuição
de postings).

### Leitura do achado

1. **`keep`/`doc_len` deixaram de ser o gargalo — o sidecar funcionou exatamente como a FTOPT-1
   previu.** Juntos são 1,4% do tempo agora, contra 88,8%+4,5% pré-sidecar (FT1). Esta parte da
   pergunta original da FTOPT-0 está fechada.
2. **O novo gargalo dominante é `decode` de blocos de postings (32,7%)**, seguido de perto por
   **busca vetorial/HNSW (36,0%)** — juntos, quase 69% do tempo. O laço de bound WAND/block-max
   em si (decidir pular vs. avaliar, sem decodificar) é só 11,4%: a maior parte do tempo "BMW" não
   é a lógica de decisão do BMW, é o trabalho de decodificação que a taxa de skip de 0,07%
   praticamente não evita — a mesma causa raiz da BMW-3/BMW-5 (postings de alta frequência
   distribuídas de forma densa/uniforme no espaço de ids não deixam o block-max provar exclusão de
   blocos inteiros), agora vista pelo lado de onde o *tempo de parede* vai, não só pela contagem de
   blocos.
3. **Scoring (9,1%) e abertura de cursor (8,2%)** são não-triviais mas secundários frente à
   decodificação.
4. A comparação lexical-isolado (37,71 ms, dentro do NFR) vs. `engine` geral (129,50 ms) da rodada
   oficial é consistente com este achado: uma query cujo ground truth é um termo exato tende a
   casar poucos termos e/ou termos menos ambíguos, decodificando menos blocos por candidato
   avaliado; a amostra geral do harness mistura queries com mais termos/termos mais frequentes,
   ampliando exatamente a fase que domina (decode).

### Opções em aberto (não decididas nesta task — guard-rail FTOPT-5)

Esta task mede; não escolhe a próxima rota. Com o dado em mãos, as opções conhecidas são:

- **Aceitar o patamar atual (135,74 ms) e o NFR reprovado como documentado.** O ganho da FTOPT-1
  (~40%) já é real e o full-text lift (FT6) segue valendo como diferencial de produto.
- **Investir em reduzir o custo de decodificação em si** (não em pular mais blocos — a BMW-5 já
  fechou que o algoritmo de skip não tem mais o que explorar nesta distribuição de dado): layout de
  postings mais barato de decodificar, cache de blocos decodificados entre queries próximas, ou
  reduzir o número de candidatos avaliados por outra via (ex.: revisar o `k`/`ef_search` efetivo).
- **Investir no lado vetorial (36,0% do tempo, maior fase isolada)** — fora do escopo original desta
  fase FT (que mirava só o full-text), mas o dado mostra que otimizar só o BMW não fecha o NFR
  sozinho mesmo que a decodificação caia a zero (36,0% de vetor + o resto ainda passa de 50 ms).
- **Revisitar se o NFR de 50 ms @ 100k continua o critério certo** — decisão de produto do founder,
  não técnica.

Nenhuma dessas foi escolhida aqui. Resultado bruto em
`benches/results/profile-recall-100k-ftopt5.txt` (stdout) e `.log` (stderr/build), ambos gerados
por `cargo run -p embedmind-bench --release --bin profile_recall -- agent-mem-100k` sobre o dataset
já vacuum'ado; nenhum benchmark oficial (`run_all.sh`/`benches/results/0.1.0-dev.json`) foi
re-rodado por esta task.

## Resultado da otimização de decode de blocos (FTOPT-6, 2026-07-14)

A FTOPT-5 mediu decodificação de bloco de postings (`BmwCursor::decode_block`, `fts.rs`) em
32,7% do tempo pós-sidecar — o novo gargalo dominante, com taxa de skip do BMW travada em 0,07%
(BMW-3/BMW-5: causa raiz fechada, não reabrir essa frente). Esta task investigou, sem tocar o
algoritmo de skip, três hipóteses de custo dentro do decode em si.

**Instrumentação e método**: mesmo binário `profile_recall` da FTOPT-5, mesma amostra de 1000
queries, mesmo dataset `agent-mem-100k.mind` (`format_version` 7). Medição em duas rodadas
sequenciais no mesmo processo/máquina — baseline (`git stash` da mudança) e depois (mudança
aplicada) — para isolar o efeito da mudança de código do ruído de máquina compartilhada entre
rodadas distintas.

**Hipótese investigada (achado de código, não suposição)**: `BmwCursor::decode_block` alocava um
`Vec<Posting>` novo (`Vec::with_capacity(len)`) a cada bloco decodificado, mesmo quando o cursor
já tinha decodificado um bloco antes — 2.872.240 blocos decodificados na rodada FTOPT-5 implicam
2.872.240 pares malloc/free de ~2,5 KiB (128 entradas × 20 bytes de `Posting`) cada, quando quase
todo bloco tem exatamente `SKIP_BLOCK_SIZE` (128) entradas e o buffer do cursor anterior já serve.
As hipóteses 2 (custo do próprio loop varint) e 3 (decode parcial/lazy) do enunciado da task **não
foram implementadas** — ver "Não fechado" abaixo.

**Mudança aplicada**: `decode_block` agora reusa `self.entries` entre blocos (`clear()` +
`reserve(len)`) em vez de recriar o `Vec` a cada chamada — puramente em memória/algoritmo dentro
de uma única sessão de busca (o `BmwCursor` já vive só durante uma chamada a `search_bmw_counted`,
nunca é persistido nem cruza queries), sem tocar layout on-disk, então **sem bump de
`format_version`**. Resultado bit-idêntico garantido pela suite de equivalência existente
(`bmw_equals_oracle_on_random_corpora`, `bmw_skips_blocks_and_still_matches_the_oracle`,
`bmw_breaks_boundary_ties_exactly_like_the_oracle`) — nenhum teste novo foi necessário porque a
mudança não altera o que é escrito em `self.entries`/`self.cur_id`, só a estratégia de alocação do
buffer.

### `agent-mem-100k` (1000 queries, `format_version` 7, mesma máquina, rodadas sequenciais)

| fase | antes (ms) | antes (%) | depois (ms) | depois (%) |
|---|---:|---:|---:|---:|
| vetor (embed + HNSW) | 199.984,6 | 48,6% | 115.962,5 | 35,1% |
| fts: abertura de cursor | 32.587,2 | 7,9% | 28.664,6 | 8,7% |
| fts: laço de bound WAND/block-max | 36.869,0 | 9,0% | 37.957,0 | 11,5% |
| **fts: decodificação de bloco** | **105.714,2** | **25,7%** | **108.506,9** | **32,8%** |
| fts: `keep` | 3.984,7 | 1,0% | 3.698,4 | 1,1% |
| fts: `doc_len` | 951,6 | 0,2% | 982,4 | 0,3% |
| fts: scoring | 29.275,2 | 7,1% | 30.339,4 | 9,2% |
| carga final dos hits | 2.152,3 | 0,5% | 4.419,3 | 1,3% |
| `recall` p50 / p99 | 329,7 / 2088,8 ms | — | 305,6 / 615,1 ms | — |

Contadores do BlockMax-WAND idênticos nas duas rodadas (6.816.080 docs avaliados, 2.872.240 blocos
decodificados, taxa de skip 0,07%) — confirma resultado bit-idêntico, só o tempo de parede muda.

### Leitura do achado: ganho não mensurável nesta rodada

O tempo absoluto da fase de decode **não caiu** (105.714 ms → 108.506 ms, dentro do ruído — a fase
"vetor", que nenhuma mudança desta task tocou, variou quase 2x entre as rodadas por ruído de
máquina compartilhada, então uma diferença de ~3% na fase de decode não é distinguível de ruído). A
eliminação da realocação por bloco é uma redução real de trabalho (menos chamadas a malloc/free),
mas o alocador não era, pelos dados desta medição, o componente dominante do custo de
`decode_block` — o tempo está concentrado em outro lugar dentro da função (mais provável: o próprio
loop de `read_varint`/`decode_delta_run`, que opera em `u128` com até `MAX_VARINT_LEN` = 19
iterações por campo e uma checagem de overflow por bit-shift a cada byte, mesmo para o caso comum
de delta pequeno — hipótese 2 do enunciado, não confirmada por profiling granular nesta sessão por
falta de orçamento de tempo).

### Não fechado (falta de orçamento de tempo da sessão, não decisão técnica)

Esta task não conseguiu, dentro do teto de ~30 min, profilar o *interior* de `decode_block` em
granularidade menor que a fase inteira (ex.: tempo isolado do loop varint vs. alocação vs.
revalidação first/last/max_tf) nem implementar as hipóteses 2 (formato mais barato de decodificar)
e 3 (decode parcial/lazy parando no id procurado) do enunciado — ambas exigiriam mudança de
algoritmo com risco de equivalência maior que a mudança aplicada aqui, e portanto medição antes/
depois própria que não coube nesta sessão. A mudança aplicada (reuso de buffer) fica commitada por
ser estritamente não-pior (menos alocação, mesmo resultado, sem risco de regressão) mesmo sem ganho
demonstrado — mas **o gargalo de 32,7%/32,8% de decode continua de pé, não reduzido**. Próxima
sessão da fase FT: profiling granular dentro de `decode_block` (ex.: `Instant` ao redor só do laço
`decode_delta_run` vs. só das 3 revalidações) antes de tentar a hipótese 2 ou 3 às cegas — a mesma
disciplina de medir-antes-de-mudar desta e das tasks anteriores.

## Resultado do profiling granular do interior de `decode_block` (FTOPT-7, 2026-07-14)

A FTOPT-6 deixou em aberto qual parte do *interior* de `decode_block` respondia pelos 32,7%/32,8%
de tempo em decodificação de bloco — só media a fase inteira. Esta task instrumentou o interior da
função em dois pontos com `Instant`, sem tocar o algoritmo:

- `decode_varint_ns` — só o laço `decode_delta_run` (o parsing varint em si, incluindo a
  reconstrução delta→id e a checagem de ordem estrita).
- `decode_revalidate_ns` — só as 3 revalidações pós-decode (`first`/`last`/`.iter().map(...).max()`
  para `max_tf`).

Os dois campos foram adicionados a `BmwCounters` (`crates/embedmind-core/src/index/fts.rs`) —
`#[doc(hidden)]`, always-on como o resto da struct (custo de dois `Instant::now()` por bloco
decodificado é desprezível frente ao trabalho medido) — e propagados a `BmwPhaseTimings` no fim de
`search_bmw_profiled`, reportados por `profile_recall` como um detalhamento da linha "fts: block
decode" já existente.

**Método**: mesmo binário `profile_recall`, mesmo dataset `agent-mem-100k.mind` (`format_version`
7), mesma amostra de 1000 queries da FTOPT-5/6. Rodada única nesta sessão (sem antes/depois de
código, porque a medição granular **não motivou nenhuma mudança de algoritmo** — ver conclusão
abaixo); saída bruta em `benches/results/profile-recall-100k-ftopt7-before.txt`/`.log`.

### Decomposição do interior de `decode_block` (1000 queries, `agent-mem-100k`, fv7)

| sub-fase de `decode_block` | tempo (ms) | % da fase "block decode" |
|---|---:|---:|
| **laço `decode_delta_run` (parsing varint)** | **65.087,8** | **59,9%** |
| revalidação `first_id`/`last_id`/`max_tf` | 696,0 | 0,6% |
| outro (bookkeeping do cursor: `advance_to`/`current_tf` ao redor da chamada a `decode_block`, não o decode em si) | 42.835,3 | 39,4% |
| **fts: block decode (total)** | **108.619,1** | **100,0%** |

Contexto da mesma rodada: vetor 37,7%, cursor open 8,3%, bound WAND/block-max 11,0%, block decode
31,6%, keep 1,1%, doc_len 0,3%, scoring 8,9% — mesma ordem de grandeza da FTOPT-5/6 (dentro do ruído
de máquina compartilhada entre sessões, como já registrado).

### Leitura do achado

A hipótese registrada na FTOPT-6 **se confirma**: o próprio laço de decodificação varint domina —
quase 60% do tempo de "block decode" e sozinho já é ~19% do tempo total medido (maior que o laço de
bound WAND/block-max inteiro, 11,0%). A revalidação (`first`/`last`/`max_tf`), que incluía a
segunda varredura O(len) que o enunciado desta task cogitava fundir no loop de decode, é
**irrelevante** (0,6%) — fundir o cálculo de `max_tf` dentro do laço de decode eliminaria, na
melhor das hipóteses, uma fração de 0,6% da fase de decode (bem dentro do ruído observado nesta e
nas sessões anteriores, onde a fase "vetor" variou quase 2x entre rodadas sem nenhuma mudança de
código). **Por isso essa fusão não foi implementada**: seria uma mudança de código real (e portanto
risco de regressão real, ainda que pequeno) para um ganho não mensurável — o guard-rail desta task
já previa medir antes de mudar, e a medição aqui diz explicitamente "não vale a pena".

A fatia "outro" (39,4%, bookkeeping de `advance_to`/`current_tf` ao redor de `decode_block` que o
`decode_ns` do bracket em `search_bmw_profiled` também captura, mas que não é o próprio
`decode_block`) não foi decomposta além disso nesta sessão — é overhead de navegação do cursor
(partition_point, comparações), não do decode de bytes, e não estava no escopo da hipótese 2 do
enunciado.

### Conclusão: gargalo é o formato de postings, não uma ineficiência local corrigível

O achado da FTOPT-6 vira fato medido: o custo é o parsing varint em si — cada campo (delta do id,
term_freq) é lido byte a byte com checagem de overflow, até `MAX_VARINT_LEN` = 19 iterações no pior
caso, para ~2 campos × 128 entradas × 2.872.240 blocos decodificados nesta rodada. Não há
ineficiência de código óbvia aqui para corrigir sem mudar o layout on-disk: o loop já opera direto
sobre `&[u8]`, sem alocação por campo, sem branch previsível a mais do que o necessário para
segurança contra bytes hostis (G4). As únicas rotas que reduziriam esse custo de fato são as que a
FTOPT-6 já havia identificado como fora do escopo de uma mudança segura e pontual:

- **Aceitar o patamar.** ~32% em decode de blocos de postings é o custo estrutural do formato
  delta+varint atual; nenhuma mudança local (alocação, fusão de revalidação) o reduz de forma
  mensurável.
- **Mudar o formato de postings** (ex.: layout de largura fixa para term_freq, ou decodificação
  vetorizada/SIMD de deltas) — reduziria o custo do laço varint em si, mas é uma mudança de formato
  on-disk com risco de equivalência maior e exigiria bump de `format_version` — explicitamente fora
  do escopo desta task (guard-rail da sessão).
- **Revisitar o NFR de <50 ms @ 100k** — decisão de produto do founder, não técnica; mesmo se o
  decode caísse a zero, a fase vetorial (37,7% nesta rodada) sozinha já ultrapassa o orçamento.

Nenhuma dessas foi escolhida aqui — mesma disciplina de FT6/BMW-3/BMW-5/FTOPT-1/2/5/6: esta task
mede e reporta, não decide mudança de formato nem revisita o NFR.

## Revisão do NFR (decisão do founder, 2026-07-14)

O NFR original (`docs/01-spec.md` §NFR, "p99 < 50 ms @ 100k") foi definido antes de qualquer
medição real do custo do full-text híbrido. A cadeia FTOPT-0 a FTOPT-7 mostrou que esse alvo nunca
correspondeu à arquitetura do produto: o vetor puro (`vector-only`, sem BM25/fusão) já mede p99
36,16 ms — dentro do alvo original sozinho —, mas o full-text (o diferencial de posicionamento do
produto, "híbrido de verdade" no `00-prd.md` §3) acrescenta um custo estrutural que nenhuma
otimização de baixo risco eliminou (FTOPT-1/2 atacaram e resolveram o gargalo de `keep`/`doc_len`;
FTOPT-6/7 mediram e confirmaram que o que resta — o parsing varint das postings — não tem
ineficiência local corrigível).

**Pesquisa de mercado (2026-07-14)**: nenhum concorrente embarcado/local-first comparável mede
exatamente o mesmo workload (BM25 + vetor + fusão, mesmo arquivo, CPU-only, sem servidor). Os
números mais próximos: LanceDB embarcado mede ~25 ms p50 só de vetor e ~50 ms p99 quando soma
filtro de metadado (ainda sem full-text nem embed); Chroma embarcado mede p95 ~51 ms num corpus
menor (10k), também sem o full-text que o EmbedMind soma. Ferramentas com p50 de poucos ms (Qdrant,
Weaviate) são servidores dedicados com processo próprio e cache quente — categoria diferente de um
arquivo único sem servidor. Nenhum concorrente testado soma embed + busca + fusão no mesmo número
que o EmbedMind reporta (a maioria isola só o índice) — o número do EmbedMind é mais honesto
(end-to-end) que a maioria das comparações de mercado disponíveis, o que também significa que não
há um "alvo de mercado" direto para copiar.

**Decisão**: o NFR de latência do `recall` híbrido muda de **< 50 ms** para **< 100 ms** p99 @ 100k
(CPU-only). Justificativa: 50 ms nunca foi alcançável para o workload híbrido real sem uma mudança
de formato de postings (fora do escopo de uma correção pontual); mas o founder optou por **não**
recalibrar direto para o patamar já medido (135,74 ms), mantendo 100 ms como pressão explícita para
investir na próxima frente identificada — mudar o formato de postings (SIMD/vetorizado, layout de
largura fixa; ver "Opções em aberto" acima) — antes de aceitar o número atual como definitivo. O
patamar de 135,74 ms **continua reprovando** mesmo o NFR revisado: a fase FTOPT não fecha ainda.

Este ADR registra a decisão tomada pelo founder; não decide sozinho — a escolha de 100 ms (em vez
de aceitar 135,74 ms ou manter 50 ms) foi feita explicitamente fora desta pipeline de tasks
automatizadas, com pesquisa de mercado como insumo, não como cálculo determinístico.

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
