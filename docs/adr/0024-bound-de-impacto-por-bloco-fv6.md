# ADR 0024 — Bound de impacto por bloco no skip index FTS (`format_version` 6)

**Status:** Aceito (jul/2026). Executa **BMW-1**, o primeiro passo da fase
BlockMax-WAND ([ADR 0023](0023-blockmax-wand-decisao-fase-bmw.md)): dá ao skip
index do fv5 ([ADR 0022](0022-postings-fts-skip-lists.md)) o **upper bound de
impacto por bloco** que o BMW precisa para pular um bloco inteiro sem
decodificá-lo. Outro bump **aditivo** de `format_version` (mesmo padrão dos
ADRs 0021/0022): arquivo de versão anterior continua legível no seu layout.

## Contexto

O ADR 0023 decidiu investir na reescrita BlockMax-WAND para fechar o NFR
`recall p99 @ 100k < 50 ms`, sobre a estrutura de skip index já entregue pelo
ADR 0022. O BMW pula um bloco de postings quando prova que **nenhuma** entrada
dele pode entrar no top-k — isto é, quando o `max_impact` do bloco é estritamente
menor que o k-ésimo melhor score corrente. Para isso o skip entry precisa
carregar, por bloco, o par padrão do algoritmo:

- **`block_max_impact`** — um *upper bound* do score BM25 parcial de qualquer
  entrada do bloco.
- **`block_max_docid`** — o maior `record_id` do bloco, para o cursor WAND
  avançar por range de id sem tocar nas entradas.

O skip entry do fv5 (24 bytes) já tem `first_id` (o *menor* id do bloco),
`byte_offset` e `max_term_freq`. Faltam, para o BMW, o `block_max_docid`
(o *maior* id) e a formalização de que `max_term_freq` **é** o impact bound.

## A avaliação central: `max(tf)` basta? `min(doc_len)` é preciso?

O escopo da task manda avaliar se o upper bound do score BM25 parcial exige,
além de `max(tf)`, também o `min(doc_len)` do bloco. **O bound TEM de ser
conservador — nunca subestimar** —, senão o BMW descarta um bloco com resultado
válido e quebra a equivalência bit-idêntica que a fase exige.

O score BM25 parcial de um termo num documento é

```
score = IDF · tf·(k1+1) / ( tf + k1·(1 − b + b·|D|/avgdl) )
```

Essa função é **monotonicamente crescente em `tf`** e **decrescente em `|D|`**
(o denominador cresce com `|D|`). Logo o máximo do bloco está em
`(max tf, min |D|)`. Como `|D| ≥ 1 > 0` sempre, avaliar em `|D| → 0` (denominador
mínimo `tf + k1·(1−b)`) com `tf = max_term_freq` é um **upper bound
comprovadamente conservador**: nunca fica abaixo do score real de nenhuma
entrada do bloco. **Este é exatamente o bound que a passada 1 de `fts::search`
já computa** hoje (`crates/embedmind-core/src/index/fts.rs`, o acúmulo de bounds
usa `norm = tf + k1·(1−b)`), então usar `max_term_freq` como impact bound do
bloco não muda nenhum resultado — só o torna disponível por bloco, sem decodificar.

**`min(doc_len)` daria um bound mais apertado — mas mais apertado, não mais
correto —, e não é derivável do corpo de postings.** O comprimento `|D|` **não é
persistido** em lugar nenhum (§11 "Scoring": é recomputado tokenizando o conteúdo
do candidato no query time, decisão do ADR 0011/DESIGN para que nada sobre `|D|`
possa divergir no disco). O corpo de postings só conhece `{record_id,
term_freq}`; o encoder de blocos não tem como saber o `|D|` de cada documento —
as postings de um bloco são mescladas de muitas escritas `index_document` ao
longo do tempo, e nenhuma delas gravou `|D|` por posting. Guardar `min(doc_len)`
por bloco exigiria **persistir `doc_len` por posting**, o que:

1. quebra a invariante "`|D|` não fica no disco" (§11 Scoring, ADR 0011) —
   território governado pelo founder (decisão-3), não uma mudança de formato de
   rotina; e
2. contamina o tipo `Posting`, compartilhado pelos três layouts, com um campo
   que só o fv6 saberia preencher (fv≤5 decodificaria com `doc_len` desconhecido,
   quebrando o round-trip).

Custo grande, invariante quebrada, e o ganho é só um bound mais justo — não a
correção. **Rejeitado.** O impact bound do fv6 é `max_term_freq` (já presente),
usado com `|D| → 0`.

## Decisão

### 1. Skip entry v6: adiciona `last_id` (block max doc id) — `format_version` 6

A partir de `format_version` **6**, cada entrada do skip index de um corpo de
postings grande passa a ter **40 bytes** (spec normativa: [FORMAT.md](../FORMAT.md)
§11):

- `first_id` (u128, 16 LE) — inalterado (block *min* doc id).
- **`last_id`** (u128, 16 LE) — **novo**: o maior `record_id` do bloco. Como a
  lista é estritamente ascendente, é o id da última entrada do bloco. É o
  `block_max_docid` do BMW.
- `byte_offset` (u32) — inalterado.
- `max_term_freq` (u32) — inalterado; é o `block_max_impact` (avaliado em
  `|D| → 0`), conforme a avaliação acima.

O caso pequeno (`block_count = 0`, termo com < `SKIP_MIN_DOC_FREQ` entradas)
segue **byte-idêntico** ao fv4/fv5: nenhum skip index, corpo delta+varint puro.

### 2. Seleção de layout pela versão do arquivo, nunca por corpo

Mesma regra dos ADRs 0021/0022. O layout (largura do skip entry inclusive) é
função do `format_version` do header. Um build v6 abrindo arquivo v≤5 **lê e
escreve** no layout daquele arquivo (skip entry de 24 bytes para v5, skip-less
para v4, fixed-width para v≤3). Migração = o rebuild por cópia do `vacuum`, que
re-codifica no layout corrente. Um arquivo nunca mistura larguras de entry.

### 3. O algoritmo de busca **não** muda nesta task

Separação deliberada, igual à do ADR 0022 §5: esta task entrega **só formato +
bound**. A passada 1 de `fts::search` continua linear (materializa a lista
inteira). Ligar o `(last_id, max_term_freq)` ao hot path — pular blocos de
verdade — é a task seguinte da fila BMW (ADR 0023). Assim, se o BMW atrasar, o
formato v6 já está estável, testado e fuzzado. Encode/decode continuam bijeções
sobre a mesma `Postings` em memória; nenhum resultado de busca muda.

### 4. Decoder defensivo; `last_id` é auditado, não confiado

O decoder do v6 rejeita tudo o que o v5 já rejeitava, mais um `last_id` gravado
que divirja do último id decodificado do bloco (`"fts skip last_id mismatch"`).
Cada campo do skip entry — `first_id`, `last_id`, `byte_offset`, `max_term_freq`
— é **re-derivado das entradas do bloco e conferido** contra o gravado. Toda
aritmética sobre bytes do arquivo é *checked*/`malformed`, nunca panic (lição do
crash de `lookup_via_skip`, ADR 0022 / [FORMAT.md](../FORMAT.md) G4). O fuzz
`fuzz_fts_page` decodifica cada input sob os **quatro** layouts (fixed-width,
delta+varint, skip v5, skip v6) e roda `lookup_via_skip` sob **ambas** as
larguras de entry sobre os mesmos bytes hostis — um corpo v6 reinterpretado como
v5 (e vice-versa) tem de nunca dar panic. Corpus ganhou seeds `-v6`
(`seed-type-08/09-v6`, `seed-postings-skip-v6` com `block_count > 0`); os seeds
`-v5`, `-v4` e fixed-width legados ficam para não perder cobertura de branch.

## Alternativas rejeitadas

- **Guardar `min(doc_len)` por bloco para um bound mais apertado:** exige
  persistir `doc_len` por posting, quebrando a invariante "`|D|` não fica no
  disco" (§11 Scoring / ADR 0011, governança do founder) e contaminando o tipo
  `Posting` dos três layouts. Ganho é bound mais justo, não correção — o
  `max_term_freq` com `|D| → 0` já é conservador e é o mesmo bound da passada 1.
  Ver "A avaliação central" acima.
- **Não bumpar o formato; documentar `max_term_freq` do fv5 como o impact
  bound:** o BMW precisa do `block_max_docid` (maior id do bloco) para avançar o
  cursor por range sem decodificar; o fv5 só guarda `first_id` (menor id). Sem
  `last_id`, o BMW não consegue pular um bloco por id. O bump aditivo é o custo
  de adicionar esse campo.
- **Tag de largura de entry por corpo de postings:** mesma rejeição dos ADRs
  0021/0022 — arquivos de estado misto, piores de auditar e fuzzar; o `vacuum`
  já é o caminho de migração por cópia.

## Consequências

- `FORMAT_VERSION` passa a 6. Um leitor v5 diante de arquivo v6 recusa abrir
  read-write (política G4); pode abrir read-only, pois o layout do header não
  mudou (nenhum campo novo, nenhum page type novo).
- Arquivos v≤5 continuam legíveis e graváveis no seu próprio layout, sem upgrade
  silencioso; o round-trip cross-version cobre v4 (delta+varint skip-less) e v5
  (skip entry de 24 bytes) sob este build.
- Cada bloco custa +16 bytes (o `last_id`) no skip index sobre o fv5 — o skip
  index sobe de ~1–4% para ~2–7% do corpo de um termo grande; barato ante o corte
  de trabalho que o BMW vai destravar. O caso pequeno não paga nada (idêntico ao
  fv4/fv5).
- O crash harness exercita páginas FTS v6 porque toda `remember` escreve postings
  no layout v6 na mesma transação do record; o skip index de `block_count > 0` é
  coberto por round-trip determinístico, pelo seed de fuzz `seed-postings-skip-v6`
  e pelo fuzz do parser.
- **BMW-2** (ligar o bound ao hot path via BlockMax-WAND) e a medição @ 100k que
  fecha o NFR são as próximas tasks da fase BMW ([ADR 0023](0023-blockmax-wand-decisao-fase-bmw.md)).
