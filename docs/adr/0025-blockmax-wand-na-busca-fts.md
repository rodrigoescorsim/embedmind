# ADR 0025 — BlockMax-WAND na busca full-text (BMW-2)

**Status:** Aceito (jul/2026). Executa **BMW-2**, o coração da fase BlockMax-WAND
([ADR 0023](0023-blockmax-wand-decisao-fase-bmw.md)): substitui a acumulação
linear de bounds da passada 1 de `fts::search` por **BlockMax-WAND** sobre o
bound de impacto por bloco do `format_version` 6
([ADR 0024](0024-bound-de-impacto-por-bloco-fv6.md)). Arquivos v4/v5 (sem o
bound por bloco) continuam no caminho linear antigo, intacto.

## Contexto

Depois do FT2/FT3 (ADRs 0018/0021/0022), o gargalo restante do `recall p99 @
100k` é a **passada 1** de `fts::search`: ela decodifica *todas* as postings de
*todos* os termos casados (~366k pares/query @ 100k) para acumular um upper
bound por candidato, antes de a passada 2 avaliar poucos candidatos exatamente.
O skip index existe desde o fv5, mas só cortava trabalho no `lookup_via_skip`
(ponto único); a busca continuava linear. O fv6 (BMW-1) adicionou o par
`(last_id, max_term_freq)` por bloco — exatamente o `(block_max_docid,
block_max_impact)` que o BlockMax-WAND precisa para **pular blocos inteiros sem
decodificá-los**.

## A restrição que molda o algoritmo

**A garantia muda de "bit-idêntico por construção" para "top-N idêntico
verificado por teste"** — mas o resultado tem de ser o MESMO. O top-N do BM25
alimenta a fusão RRF do `Store::recall`; top-N igual ⇒ híbrido igual.

Uma tentativa ingênua — manter as duas passadas e fazer a passada 1 pular
blocos usando o *k-ésimo melhor bound* como threshold — está **errada**: um
candidato de bound baixo pode ter score exato alto (ex.: `k=1`, candidato A com
bound 10/score 2 e candidato B com bound 5/score 5 — o threshold-por-bound 10
descartaria B, o verdadeiro top-1). O threshold correto do WAND é o **k-ésimo
score EXATO do heap corrente**, que só existe se a avaliação exata acontecer
*durante* a varredura. Logo as duas passadas se fundem num único laço
**document-at-a-time (DAAT)** — o desenho clássico do BMW (Ding & Suel 2011).

## Decisão

### 1. `search` despacha por versão; o linear vira oráculo

- `fts::search` em arquivo **fv ≥ 6** roda `search_bmw_counted` (BlockMax-WAND).
- Em arquivo **fv ≤ 5** roda `search_linear` — o caminho FT2 inalterado (duas
  passadas), que também é o **oráculo de referência** dos testes de
  equivalência, ao lado do scan exaustivo `search_profiled` (FT1). Não fundir
  os caminhos: o dia em que o linear morrer, morre junto o detector de bugs
  silenciosos de recall do BMW.

### 2. O algoritmo (DAAT WAND + refinamento block-max)

Um `BmwCursor` por termo casado, criados **em ordem de termo ordenada** (a
mesma do oráculo). O cursor navega pelos metadados do skip index e decodifica
**só os blocos em que o pouso cai dentro**; pousar no `first_id` de um bloco
não decodifica nada (o skip entry já carrega o id). Lista pequena
(`block_count = 0`) decodifica inteira e vira um bloco sintético — o WAND
funciona igual.

Laço principal, com `θ` = k-ésimo score exato do heap (quando cheio):

1. **Pivô WAND:** cursores ordenados por id corrente; o pivô é o primeiro
   prefixo cuja soma de `term_ub` (bound global do termo, o máximo dos bounds
   de bloco) pode bater `θ`. Sem pivô ⇒ nada mais pode entrar no top-k ⇒ fim.
   Cursores *após* o pivô parados no mesmo id entram no prefixo (contribuem
   para o mesmo documento).
2. **Refinamento block-max:** re-limita o documento-pivô pela soma dos bounds
   **dos blocos** que podem contê-lo (primeiro bloco com `last_id ≥ pivô` de
   cada cursor do prefixo). Se nem isso bate `θ`: **pula** — avança todos os
   cursores do prefixo para `min(last_id dos blocos cobrindo) + 1` (limitado
   pelo id do próximo cursor fora do prefixo), sem decodificar bloco algum.
   Todo documento na faixa pulada está provadamente abaixo de `θ`: seus termos
   estão todos no prefixo e a contribuição de cada um é limitada pelo bound do
   mesmo bloco coberto.
3. **Alinhamento/avaliação:** se os cursores do prefixo não estão todos no
   pivô, avança os atrasados até ele (documentos saltados estão cobertos pelo
   argumento do prefixo — a soma de bounds de qualquer sub-prefixo é ≤ `θ`).
   Alinhados, avalia o documento **exatamente**: `keep` → `doc_len` → score
   BM25 com a MESMA expressão f32 e a MESMA ordem de termos do oráculo (scores
   bit-idênticos), inserção no top-k com a MESMA regra de fronteira
   `(score desc, record_id asc)`.

### 3. Os dois riscos conhecidos, e como cada um é fechado

**Empates na fronteira.** O desempate é determinístico por `record_id`
ascendente **antes do corte, nos dois caminhos** (a `partition_point` de
inserção é idêntica à da passada 2 linear). No DAAT a avaliação é estritamente
crescente em id, então um candidato pulado com score igual a `θ` *perderia* o
empate de qualquer forma: todo item do heap com aquele score entrou antes (id
menor). Por isso o skip com `bound ≤ θ` (não estrito) é seguro — provado no
código e batido pelo teste de fronteira com corpus de documentos idênticos.

**Bound que subestima por arredondamento f32 (bug silencioso de recall).** O
oráculo soma contribuições f32 em ordem de termo; o BMW soma bounds em ordem
de documento. Somas f32 em ordens diferentes arredondam diferente — a
monotonicidade par-a-par (bound ≥ contribuição exata, herdada do ADR 0024) não
basta para a *soma*. Solução: toda comparação de bound do BMW acumula em
**f64** e aplica a folga multiplicativa `bound_slack(m) = 1 + m·1,2×10⁻⁷`
(uma soma f32 de `m` termos não-negativos excede a soma real por no máximo
`(1+2⁻²⁴)^(m−1)`; o erro do acumulador f64 é ordens de grandeza menor). Um
documento só é pulado quando `soma_f64 · slack ≤ θ` — ou seja, quando nem a
soma f32 do oráculo, no pior arredondamento, alcançaria `θ`. Custo: a folga
(~10⁻⁶ relativo) avalia raríssimos candidatos a mais; nunca a menos.

### 4. Leitura defensiva no novo caminho rápido

Lição do crash de `lookup_via_skip` (ADR 0022): **todo caminho de leitura novo
replica todas as validações do decoder completo.** `BmwCursor::open` valida o
cabeçalho como `decode_delta_varint_skip` (contagens, tamanho do índice) e, no
nível de metadado, ids ordenados dentro e entre blocos, offsets estritamente
crescentes a partir de 0 e `max_term_freq > 0`; cada bloco decodificado é
re-conferido contra seu skip entry (`first_id`/`last_id`/`max_term_freq`).
Aritmética checked/`malformed`, nunca panic (G4) — incluindo `id + 1` em
`u128::MAX`, que sem `checked_add` seria loop infinito. `fuzz_decode_page`
agora também abre e caminha um `BmwCursor` sobre os mesmos bytes hostis.

### 5. Instrumentação para a BMW-3

`BmwCounters` (`blocks_total`/`blocks_decoded`/`blocks_skipped()`/
`docs_evaluated`/`pivot_skips`) sai de `search_bmw_counted` — superfície
`#[doc(hidden)]` no padrão do `search_profiled` (FT1): produção usa `search` e
descarta; o custo permanente são quatro incrementos de u64. A BMW-3 mede com
isso quanto o BMW corta de fato @ 100k.

## Suite de equivalência (o critério de pronto)

Todos comparam **hits completos** — ids, scores (via `==` de f32, bit-exato) e
ordem — do BMW contra `search_profiled` E `search_linear`:

- **Corpus determinístico grande** (`early_termination_matches_exhaustive_...`,
  agora tripla): > `SKIP_MIN_DOC_FREQ` docs, termos comuns+raros, empates,
  filtro `keep`, records sumidos, k = 1/3/10/500.
- **Fronteira de empate** (`bmw_breaks_boundary_ties_...`): corpus inteiro de
  documentos idênticos (scores todos iguais) com skip index real; o corte tem
  de ser exatamente id-ascendente em todos os k.
- **Property-based** (`bmw_equals_oracle_on_random_corpora`, proptest):
  corpora, queries, máscaras de `keep` e de registros sumidos arbitrários —
  o detector dos dois riscos acima.
- **Contadores** (`bmw_skips_blocks_...`): corpus onde um documento curto
  domina; afirma `blocks_skipped > total/2`, `pivot_skips > 0` e avaliação
  confinada ao bloco dominante — o BMW pula DE FATO, não só devolve igual.
- **Fuzz:** `fuzz_fts_page` cobre o cursor; corpus de seeds v4/v5/v6 intacto.

`Store::recall`/`search_text` não mudam: consomem `fts::search`, cujo top-N é
idêntico ao oráculo pela suite acima — híbrido idêntico por composição.

## Alternativas rejeitadas

- **BMW na passada 1 mantendo a passada 2:** threshold por bound descarta
  candidato válido (contraexemplo acima). Incorreto, não só arriscado.
- **Comparações de bound em f32 puro, sem folga:** o proptest é exatamente o
  teste que flagra o dia em que uma soma reordenada arredonda para baixo do
  score de fronteira. Folga de 10⁻⁶ é mais barata que um bug de recall
  silencioso.
- **Avaliar exato em ordem de melhor-bound (como o FT2) dentro do WAND:**
  quebra a invariante DAAT de ids crescentes que torna o desempate por id
  provadamente idêntico ao oráculo; a economia de `keep`/`doc_len` que o FT2
  buscava continua existindo no BMW porque `θ` sobe rápido com o heap.

## Consequências

- Nenhuma mudança de formato; fv6 (ADR 0024) já carregava tudo. Arquivos v4/v5
  seguem no linear — sem bound por bloco não há como pular com segurança.
- A passada 1 deixa de ser O(postings totais) em fv6; quanto corta na prática
  é a medição da **BMW-3** (`benches/run_all.sh --full` @ 10k/100k, dataset
  regenerado em fv6), que fecha ou não o NFR `< 50 ms` e alimenta a decisão da
  BMW-4 (ADR 0023 "Critério de reversão").
- `search_linear` fica permanentemente como oráculo + caminho legado; qualquer
  mudança futura de scoring precisa tocar os DOIS caminhos e passar a suite.
