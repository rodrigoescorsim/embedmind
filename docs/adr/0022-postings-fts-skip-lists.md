# ADR 0022 — Skip lists nas postings FTS grandes (`format_version` 5)

**Status:** Aceito (jul/2026). Executa o passo 3 da ordem de risco do
[ADR 0017](0017-otimizacao-do-full-text-escopo-e-metodo.md) §3 — o corte
assintótico de verdade nas postings —, usando outro bump aditivo de
`format_version` (o mesmo padrão do [ADR 0021](0021-postings-fts-delta-varint.md)):
arquivo de versão anterior continua legível no seu próprio layout.

## Contexto

O gate de entrada da S26 parte 2 era o benchmark oficial pós-delta+varint: se o
NFR `recall p99 @ 100k < 50 ms` já tivesse fechado com a compressão (ADR 0021)
+ early termination (ADR 0018), esta otimização seria adiada — não implementada
por princípio (ADR 0017 §3: "a lista é a sequência de investigação, não um
compromisso de implementar todas").

**Não fechou.** O founder regenerou os datasets em `format_version` 4 (header
`MINDFMT1 04 …` de `benches/data/agent-mem-100k.mind`, confirmado byte a byte)
e rodou a suíte sobre eles. A rodada oficial (`benches/results/0.1.0-dev.json`,
`date_utc` 2026-07-13, sobre o `.mind` fv4) mediu:

| métrica @ 100k | valor | NFR |
|---|---:|:---:|
| recall p99 (`recall`, com embed) | **224,88 ms** | < 50 ms ❌ |
| ↳ query engine p99 (sem embed) | 219,55 ms | — |
| ↳ query vector p99 (só HNSW) | 29,32 ms | — |
| ↳ query embed p99 | 6,21 ms | — |

O vetorial puro passa com folga (29 ms); o embed é 6 ms. Os ~190 ms restantes
do engine são o meio full-text — o mesmo gargalo que o ADR 0017 isolou. A
compressão fv4 reduziu bytes por entrada mas não mudou a ordem assintótica: a
passada de bounds de `fts::search` continua percorrendo **todas** as postings
dos termos casados (~366 mil por query @ 100k, medição da FT1). A S26 parte 2
prossegue.

## Decisão

### 1. Skip index no corpo de postings grandes (`format_version` 5)

A partir de `format_version` **5**, o corpo de postings de um termo com pelo
menos `SKIP_MIN_DOC_FREQ` (= 512) entradas ganha um **skip index** prefixado.
Layout do corpo (spec normativa: [FORMAT.md](../FORMAT.md) §11):

- `doc_freq` (u32) — inalterado.
- `block_count` (u32). **`block_count = 0`** = lista pequena, sem skip index:
  segue o corpo delta+varint idêntico ao fv4 após esse prefixo. Custo de 4
  bytes sobre o fv4 para o caso pequeno.
- `block_count > 0`: o skip index — `block_count` entradas de `first_id`
  (u128, 16 bytes LE) · `byte_offset` (u32, relativo ao início da região de
  blocos) · `max_term_freq` (u32) — seguido da região de blocos.
- Cada bloco tem até `SKIP_BLOCK_SIZE` (= 128) entradas delta+varint e
  **re-baseia sua cadeia de deltas** (`prev = 0` no início do bloco: o primeiro
  delta é o id absoluto), então um bloco decodifica sozinho, sem varrer os
  anteriores.

**Por que blocos de 128 e limiar de 512:** medição no corpus de teste. A
entrada de skip custa 24 bytes fixos; com blocos de 128 entradas de ~2–14 bytes
cada, o índice fica em torno de 1–4% do corpo — barato o bastante para não
apagar o ganho de compressão do ADR 0021. Abaixo de 4 blocos (512 entradas) o
índice custa mais bytes e desvios do que o scan linear economiza, então listas
menores mantêm o corpo skip-less (`block_count = 0`). Os dois números são
`const` no código (`SKIP_BLOCK_SIZE`, `SKIP_MIN_DOC_FREQ`) e revisáveis por
medição futura sem mudar o formato (o decoder deriva a estrutura do
`block_count` e do `doc_freq` gravados, não de uma constante do build).

### 2. Seleção de layout pela versão do arquivo, nunca por corpo

Mesma regra do ADR 0021: o layout é função do `format_version` do header. Um
build v5 abrindo arquivo v≤4 **lê e escreve** no layout daquele arquivo
(fixed-width para v≤3, delta+varint skip-less para v4), então o arquivo
continua legível pelo build que o criou. Migração = o rebuild por cópia do
`vacuum`, que re-codifica as postings no layout corrente. Um arquivo nunca
mistura layouts.

### 3. Lookup por bloco, verificado contra o scan linear

`lookup_via_skip` acha o `term_freq` de um id **sem decodificar a lista
inteira**: binary-search no skip index pelo bloco cujo range de ids pode conter
o alvo, depois decodifica só esse bloco (≤ 128 entradas). É a peça que "pula
blocos sem decodificá-los" (escopo da story). A equivalência com a busca linear
(`binary_search` sobre a lista totalmente decodificada) é um teste dedicado,
para todo id presente e para ausentes (antes do primeiro, depois do último, nos
buracos entre ids consecutivos).

### 4. Decoder defensivo; o skip index é auditado, não confiado

O decoder do layout v5 rejeita, além de tudo o que o fv4 já rejeitava:
`block_count` que não bate com `doc_freq`/`SKIP_BLOCK_SIZE`, skip index que não
cabe no corpo, `byte_offset` que não aponta para a emenda real de um bloco, e
`first_id`/`max_term_freq` que divergem do que os bytes do bloco decodificam —
cada campo do índice é **re-derivado das entradas e conferido** contra o
gravado. A ordem estrita global é imposta na emenda entre o último id de um
bloco e o primeiro do próximo (blocos re-baseiam, então a checagem intra-run
não a cobre sozinha). O fuzz body `fuzz_fts_page` decodifica cada input sob os
**três** layouts (fixed-width, delta+varint, delta+varint+skip) e roda
`lookup_via_skip` sobre os mesmos bytes hostis, no mesmo commit da mudança de
formato (regra do 04-agents.md). O corpus ganhou seeds v5
(`seed-type-08-v5`/`seed-type-09-v5` e `seed-postings-skip-v5`, este com skip
index real, `block_count > 0`); os seeds `-v4` e os fixed-width legados ficam
para não perder cobertura de nenhum branch.

### 5. Nenhum resultado de busca muda; ganho no hot path é follow-up medido

Encode/decode são bijeções sobre a mesma `Postings` em memória; BM25, early
termination (ADR 0018) e RRF não são tocados. A equivalência continua garantida
pelo oráculo `search_profiled` e por round-trip nos três layouts — o teste de
equivalência do scan agora roda sobre um corpus acima de `SKIP_MIN_DOC_FREQ`,
provando que `search` casa com o oráculo **quando o skip index é a forma em
disco**.

**Honestidade sobre onde o ganho entra.** O `fts::search` atual não fica mais
rápido só por existir o skip index: a passada 1 (acumulação de upper bounds)
materializa a lista inteira de cada termo — ela precisa visitar cada posting
para somar o bound de cada candidato no `HashMap` — e a passada 2 então faz
`binary_search` O(log n) sobre esse `Vec` já em memória. O skip só corta
trabalho quando o decode é preguiçoso por bloco **e** a passada 1 não precisa de
todas as entradas — isto é, com um algoritmo BlockMax-WAND que pula um bloco
inteiro quando seu `max_term_freq` prova que nenhuma entrada dele entra no
top-k. Reescrever a passada 1 para BMW muda a ordem de avaliação e é risco alto
para a equivalência bit-idêntica que a fase exige; fica como task própria. Esta
task entrega a **estrutura** (formato v5 + lookup por bloco + garantias de
equivalência/crash/fuzz), que é o pré-requisito daquela — a própria story marca
"medição do ganho @ 100k fica para a task de fechamento do ADR 0017".

## Alternativas rejeitadas

- **Pular blocos já na passada 1 desta task (BlockMax-WAND agora):** cortaria o
  tempo de verdade, mas muda a ordem de avaliação dos candidatos e o critério de
  parada — arrisca divergir do oráculo `search_profiled` em regimes de empate e
  arredondamento f32 que a FT2 fechou com cuidado. Grande demais para caber com
  a mudança de formato num commit auditável; separado em task própria com o dado
  de medição em mãos.
- **Tag de layout por corpo de postings:** mesma rejeição do ADR 0021 — cria
  arquivos de estado misto, pior de auditar e fuzzar, e o `vacuum` já é o
  caminho de migração por cópia.
- **Skip index sempre, mesmo em listas pequenas:** desperdício. A maioria dos
  termos tem poucos documentos; um índice de bloco neles só adiciona bytes e um
  desvio. O limiar `block_count = 0` mantém o caso pequeno idêntico ao fv4 a
  custo de 4 bytes.

## Consequências

- `FORMAT_VERSION` passa a 5. Um leitor v4 diante de arquivo v5 recusa abrir
  read-write (política G4); pode abrir read-only se o layout maior do header não
  mudou (não mudou — nenhum campo novo, nenhum page type novo).
- Arquivos v≤4 continuam legíveis e graváveis no seu próprio layout, sem
  upgrade silencioso; o teste de round-trip cross-version cobre v3 (fixed-width)
  e v4 (delta+varint skip-less) sob este build.
- O crash harness exercita páginas FTS v5 porque toda `remember` escreve
  postings no layout v5 (o prefixo `block_count` faz parte de cada corpo, com
  `block_count = 0` para termos pequenos) na mesma transação do record; o skip
  index de `block_count > 0` é coberto por round-trip determinístico, pelo seed
  de fuzz `seed-postings-skip-v5` (que passa pelo mesmo WAL/checkpoint na
  geração) e pelo fuzz do parser — um crash test dedicado de 512+ postings seria
  redundante com o round-trip e caro no harness que reexecuta matando o
  processo, decisão consciente de não inflá-lo.
- O follow-up (BMW na passada 1) e a medição @ 100k que fecha o NFR e o ADR 0017
  são a próxima task da fase FT.
