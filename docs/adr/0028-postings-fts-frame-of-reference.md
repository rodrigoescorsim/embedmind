# ADR 0028 — Postings FTS com blocos frame-of-reference (`format_version` 8)

**Status:** Aceito (jul/2026). Executa a frente aberta pela FTOPT-8 (decisão do
founder, 2026-07-14): trocar o formato de postings para reduzir o custo de
decodificação medido, o maior gargalo restante da fase FT depois que
[ADR 0017](0017-otimizacao-do-full-text-escopo-e-metodo.md) esgotou as
otimizações locais. Bump aditivo de `format_version` sobre o layout skip do
[ADR 0022](0022-postings-fts-skip-lists.md)/[ADR 0024](0024-bound-de-impacto-por-bloco-fv6.md):
arquivo antigo continua legível e gravável no seu layout, `vacuum` é o upgrade.

## Contexto

O profiling granular da FTOPT-7 (`docs/adr/0017` §"Resultado do profiling
granular do interior de `decode_block` (FTOPT-7)") isolou o custo: o laço
`decode_delta_run` — o parsing varint LEB128 dos deltas — domina **59,9%** da
fase "block decode" (que é 31,6% do tempo total da query @ 100k), enquanto a
revalidação das entradas do bloco é ruído (0,6%). A causa raiz é o próprio
formato delta+varint (ADR 0021): cada delta é um `u128` LEB128 lido byte a byte
com um branch de bit-de-continuação por byte (até 19 iterações no pior caso), e
cada `term_freq` outro varint. São ~366 mil entradas percorridas por query
@ 100k, dois varints cada.

A indústria (Lucene, tantivy, PISA) resolve exatamente isto com **layouts de
largura fixa por bloco** (frame-of-reference / PFOR) ou decodificação
vetorizada (Lemire & Boytsov, *Decoding billions of integers per second through
vectorization*). O skip index do ADR 0022 já particiona a lista em blocos de
`SKIP_BLOCK_SIZE` = 128 entradas re-baseados (`prev = 0`) — a unidade natural
para aplicar frame-of-reference sem tocar o índice de skip nem a navegação
BlockMax-WAND.

## Decisão

1. **Codificação (só o corpo do bloco muda).** A partir de `format_version`
   **8**, dentro de um corpo de postings com skip index (`block_count > 0`) cada
   bloco deixa de ser um `decode_delta_run` intercalado e passa a ser
   **frame-of-reference com streams separados de largura fixa**:

   - `delta_width` (u8, 1..=16) — bytes por delta;
   - `tf_width` (u8, 1..=4) — bytes por `term_freq`;
   - stream de `len` deltas, cada um `delta_width` bytes little-endian, contíguo;
   - stream de `len` `term_freq`, cada um `tf_width` bytes little-endian, contíguo.

   `delta_width`/`tf_width` são o número mínimo de bytes que cobre o **maior**
   valor do bloco (frame-of-reference: um único quadro por bloco, não por
   sub-grupo). Como o skip index já re-baseia cada bloco (`prev = 0`), o
   primeiro delta é o id absoluto do bloco — que pode exigir os 16 bytes
   completos —, mas os deltas seguintes são pequenos; medir a largura pelo
   máximo do bloco custa esses 16 bytes só no bloco cujo primeiro id é grande,
   diluído por 128 entradas. O decode vira **dois laços de passo fixo sem
   branch de continuação** (vetorizáveis pelo compilador), eliminando o custo
   que a FTOPT-7 mediu.

2. **Listas pequenas mantêm delta+varint plano.** Um corpo com
   `block_count = 0` (termo com menos de `SKIP_MIN_DOC_FREQ` = 512 entradas)
   continua byte-idêntico ao layout skip fv≤7 (`doc_freq` + `0u32` +
   `decode_delta_run`). Frame-of-reference só compensa onde há blocos; abaixo
   do limiar o overhead de 2 bytes de largura por bloco não se paga e o custo
   de decode dessas listas curtas é irrelevante.

3. **Skip index e `SkipEntry::V6` reusados sem mudança.** O índice de skip
   (`block_count`, e por bloco `first_id`/`last_id`/`byte_offset`/`max_term_freq`)
   é idêntico ao fv6 — os `byte_offset` agora apontam para corpos FOR em vez de
   delta+varint, mas o formato do índice, os invariantes de ordenação e a
   navegação BlockMax-WAND (ADR 0025) não mudam. `PostingsLayout::FrameOfReference`
   carrega o mesmo `SkipEntry::V6`.

4. **Seleção por versão do arquivo, nunca por corpo.** `for_format_version`:
   `v ≥ 8` → `FrameOfReference(V6)`; `v` 6..=7 → `DeltaVarintSkip(V6)`; etc.
   (regra do ADR 0021 §2). Build fv8 abrindo arquivo fv≤7 lê **e** escreve no
   layout antigo — degrada em latência, nunca em corretude nem em erro. Upgrade
   só por `vacuum` (rebuild por cópia via `index_document`), explícito.

5. **Decoder defensivo (G4).** O decoder FOR rejeita, com erro tipado e nunca
   panic: `delta_width` fora de 1..=16, `tf_width` fora de 1..=4, streams
   truncados (largura × len contra o buffer, checado antes de alocar), delta 0
   após a primeira entrada (id duplicado/não ordenado), soma de id além de
   `u128::MAX` (checked), `term_freq` 0. O fuzz body `fuzz_fts_page` decodifica
   cada input sob o layout novo no mesmo commit da mudança (regra 04-agents.md);
   `lookup_via_skip` e `BmwCursor::decode_block` replicam **todas** as
   validações do decoder completo (lição do `lookup_via_skip` no ADR 0022).

6. **Nenhum resultado de busca muda.** Encode/decode são bijeções sobre a mesma
   `Postings` em memória; BM25, BlockMax-WAND e RRF não são tocados.
   Equivalência garantida pelo oráculo (`bmw_equals_oracle_on_random_corpora`
   e afins) e round-trip nos layouts.

## Alternativas rejeitadas

- **Decodificação SIMD explícita** (rota 2 da FTOPT-8): maior ganho potencial,
  mas exige `std::arch`/intrinsics por arquitetura (x86_64 AVX2 vs. aarch64
  NEON vs. fallback escalar), fere "Rust stable, sem nightly" na forma portátil,
  e multiplica o risco de um decoder que precisa ser à prova de bytes hostis. O
  layout de largura fixa **habilita** a auto-vetorização do compilador (loop de
  passo fixo, sem branch) sem escrever intrinsics — captura a maior parte do
  ganho com uma fração do risco. Rejeitado nesta task; reavaliável se o
  auto-vetorizado não fechar o NFR.
- **PFOR com exceções** (patched frame-of-reference: largura para o percentil,
  outliers em stream de exceção): comprime melhor que FOR puro quando um bloco
  tem poucos deltas gigantes, mas adiciona um terceiro stream e a lógica de
  patch — mais estado, mais superfície de fuzz — por um ganho de tamanho que
  não é o objetivo aqui (o objetivo é velocidade de decode, e FOR puro já
  remove o branch varint). Rejeitado por complexidade/risco.
- **Largura fixa global (por lista inteira, não por bloco)**: um único
  `delta_width` para a lista toda seria refém do maior delta de qualquer bloco.
  Frame-of-reference **por bloco** é estritamente melhor e o bloco já é a
  unidade de skip. Rejeitado.
- **Tag de layout por corpo**: cria arquivos de estado misto, pior de auditar e
  fuzzar; `vacuum` já é o caminho de migração (ADR 0003/0021). Rejeitado.

## Consequências

- `FORMAT_VERSION` = 8; arquivos novos nascem fv8. Arquivos fv≤7 seguem
  plenamente funcionais neste build, no layout deles (round-trip coberto por
  teste com arquivo fv7 genuíno via `PagerOptions.format_version`).
- `PagerOptions.format_version` validado 1..=`FORMAT_VERSION` continua sendo o
  knob dos testes de compatibilidade.
- Tamanho on-disk: FOR troca 2 varints/entrada por 2 valores de largura fixa +
  2 bytes de cabeçalho por bloco. Nos deltas ULID típicos (1–3 bytes) o tamanho
  fica próximo ao delta+varint; o ganho alvo é **velocidade de decode**, não
  bytes.
- A medição do impacto no NFR `recall p99 @ 100k` usa o mesmo
  `profile_recall`/`agent-mem-100k.mind` das FTOPT anteriores; o número honesto
  sai da medição. Se o gargalo cair mas o NFR de <100 ms não fechar, o número é
  reportado e a decisão fica em aberto (régua das tasks FTOPT).
