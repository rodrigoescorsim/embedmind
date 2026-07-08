# RELEASING — publicação no crates.io

Como publicar os crates do EmbedMind no [crates.io](https://crates.io). Cobre a
**ordem obrigatória**, os passos `[MANUAL — founder]`, e um limite do crates.io
que afeta o `embedmind-core` (modelo ONNX embarcado).

> **`cargo publish` real é ato do founder** (guardrail 7 em
> [04-agents.md](04-agents.md); item 1.6 marcado `[MANUAL — founder]` no
> [03-tasks.md](03-tasks.md)). Nenhum agente/CI publica sozinho. Este documento
> prepara tudo até a borda do `cargo publish`; o comando final é digitado por uma
> pessoa, no dia do launch.

## Os três crates

| Crate | Publica como | Papel |
|---|---|---|
| `embedmind-core` | `embedmind-core` | a engine (o ativo) |
| `embedmind-mcp` | `embedmind-mcp` | servidor MCP (casca) |
| `embedmind-cli` | **`embedmind`** | CLI + `embedmind serve` (casca) |

Nomes confirmados **disponíveis** no crates.io em jul/2026 (`embedmind`,
`embedmind-core`, `embedmind-mcp` — nenhum existe ainda no índice).

Metadados de publicação (`description`, `repository`, `homepage`, `keywords`,
`categories`, `readme`, `license = "MIT"`) já estão em cada `Cargo.toml`;
`repository`/`homepage` são herdados de `[workspace.package]`. Cada crate tem seu
próprio `README.md` (o `cargo publish` só empacota arquivos dentro do diretório
do crate — o README da raiz não é alcançável a partir de `crates/*/`).

## Ordem obrigatória: core → mcp → cli

**A ordem não é opcional.** Um crate só pode ser publicado depois que todas as
suas dependências de workspace já estão no índice do crates.io. As dependências
internas carregam `path` **e** `version` em `[workspace.dependencies]`; ao
publicar, o cargo remove o `path` e resolve pela `version` **no índice** — que só
existe após o upstream estar publicado.

```
1. embedmind-core   (sem deps internas)
2. embedmind-mcp    (depende de embedmind-core)
3. embedmind        (depende de embedmind-core e embedmind-mcp)
```

Consequência prática para verificação: `cargo publish --dry-run -p embedmind-mcp`
(e `-p embedmind`) **falha** enquanto o `embedmind-core` não estiver publicado,
com `no matching package named 'embedmind-core' found` — isso é esperado, não é
um defeito de manifesto. Para validar os três localmente **antes** de qualquer
publicação, use `cargo package --workspace` (resolve as deps internas em
conjunto):

```bash
cargo package --workspace          # empacota + compila os 3, na ordem
```

Saída esperada (jul/2026): os 3 pacotes empacotam e compilam sem erro.

## Verificação pré-publicação

```bash
# 1. Suíte verde em todas as plataformas de primeira classe (CI faz isso).
cargo test --workspace

# 2. Lints limpos (pré-condição de commit do projeto).
cargo clippy --workspace --all-targets
cargo fmt --all --check

# 3. Publish-readiness dos 3 crates, resolvendo deps internas em conjunto.
cargo package --workspace

# 4. Dry-run do core isolado (o único que resolve sem upstream publicado).
cargo publish --dry-run -p embedmind-core
```

`cargo publish --dry-run` para `embedmind-mcp` e `embedmind` só fica limpo
**depois** que o `embedmind-core` (e, para o CLI, o `embedmind-mcp`) estiverem
realmente no crates.io — ver seção da ordem acima.

## ⚠️ Limite de tamanho do crates.io (bloqueia `embedmind-core`)

O crates.io impõe um teto de **10 MiB por crate** (pacote comprimido). O
`embedmind-core` embarca o modelo ONNX quantizado + tokenizer via
`include_bytes!` (ADR [0004](adr/0004-modelo-de-embedding-embarcado.md)), então o
pacote fica em **~16 MiB comprimido**:

```
Packaging embedmind-core ...
Packaged 30 files, 23.1MiB (16.2MiB compressed)
```

O `--dry-run` **passa** localmente porque o teto é aplicado **no servidor**, no
upload real — o dry-run não sobe nada. O `cargo publish` real do `embedmind-core`
será **rejeitado** (`crate too large`) enquanto o teto padrão valer.

**Resolução (`[MANUAL — founder]`, antes de publicar o core):** solicitar
aumento de limite ao crates.io. É o procedimento padrão para crates que embarcam
modelos ML; preserva o ADR 0004 (modelo embarcado → `cargo install embedmind`
funciona sem rede, sem passo extra) e a promessa de instalação em um comando.

- Pedir via o formulário/e-mail do crates.io (help@crates.io), citando o crate
  `embedmind-core`, o tamanho (~16 MiB) e a razão (modelo de embedding embarcado,
  local-first, sem download em runtime).
- Só publicar o core **depois** de o aumento ser concedido.

Alternativas **rejeitadas** para caber nos 10 MiB (não fazer sem decisão do
founder — todas contrariam decisões vigentes):

- **Baixar o modelo em build/runtime** — reintroduz rede/dependência externa;
  contraria ADR 0004 e a promessa "nada sai da máquina" / air-gap.
- **`exclude` dos assets + fetch em runtime** — mesmo problema; quebra o
  `cargo install` out-of-the-box.
- **Modelo menor só para caber no teto** — decisão de qualidade (recall/pt-BR),
  fora de escopo aqui; já rastreada como aberta (DESIGN §12, ADR 0004/0010).

> Nota: o teto de **40 MB** do artefato de release (ADR
> [0010](adr/0010-teto-de-tamanho-governa-artefato-comprimido.md),
> `release.yml`) é um NFR **diferente** — governa o binário comprimido que o
> usuário baixa das Releases, não o pacote do crates.io.

## Smoke test de instalação (binário pré-compilado)

Antes de anunciar uma Release, valide o binário **como o usuário o recebe**: numa
pasta limpa, sem depender do checkout, exercitando o fluxo do quickstart
(`--version` → `remember` → `recall` → `stats`) sobre um `.mind` descartável. O
script [`scripts/smoke_install.sh`](../scripts/smoke_install.sh) faz isso e sai
com código ≠ 0 na primeira asserção que falhar (serve de porta de CI e de
checklist de launch).

```bash
# 1. Contra o binário baixado das Releases (o caso que importa no launch):
tar xzf embedmind-<versão>-<alvo>.tar.gz     # ou unzip no Windows
EMBEDMIND_BIN=./embedmind ./scripts/smoke_install.sh
#   Windows (bash/Git Bash):  EMBEDMIND_BIN=./embedmind.exe ./scripts/smoke_install.sh

# 2. Contra o que estiver no PATH (ex. após `cargo install embedmind`):
./scripts/smoke_install.sh

# 3. Sem binário instalado (checkout de código): faz fallback para `cargo run`.
./scripts/smoke_install.sh
```

O que o smoke test comprova, além de "roda":

- `--version` imprime `embedmind <semver>` (nome + versão bem-formados);
- `remember --global` devolve um ULID de 26 chars marcado `(global)` — grava e
  reconhece a memória;
- `recall` com uma consulta **semanticamente próxima mas de texto diferente**
  reencontra aquele id e ecoa o conteúdo — prova embedding + busca vetorial
  reais, não casamento de substring;
- `stats` reporta exatamente 1 memória viva **e** um modelo de embedding gravado
  no arquivo (`all-MiniLM-L6-v2-int8`, não `none (KV-only)`) — se o modelo ONNX
  não tivesse sido linkado no binário, esta asserção pegaria.

Tudo acontece num diretório temporário apagado na saída (`--file` próprio), então
o `~/.embedmind/memory.mind` real do founder nunca é tocado.

> `[MANUAL — founder]` complementar (fora deste script): abrir o `.mind` gerado
> num **segundo agente** (ex. Cursor) e confirmar `recall` do outro lado — a
> prova de integração ponta a ponta que precisa de um cliente MCP externo.

## Portabilidade do `.mind` entre plataformas (verificação cross-platform)

Garantia de formato **G3** ([docs/FORMAT.md](FORMAT.md) §1): o arquivo é
byte-idêntico entre plataformas porque todo inteiro multi-byte é gravado em
**little-endian fixo** — nunca na ordem nativa do host. Um `.mind` escrito no
Windows do founder abre idêntico em Linux/macOS, e vice-versa.

Coberto automaticamente por
[`crates/embedmind-core/tests/portability.rs`](../crates/embedmind-core/tests/portability.rs)
(roda em `cargo test --workspace`, portanto em toda a matriz de CI):

- `header_is_fixed_little_endian_per_format_spec` — grava um `.mind` real e
  afirma que os campos do header (page 0) estão nos offsets de FORMAT §4 **em
  little-endian explícito** (ex. `format_version = 1` são os bytes
  `01 00 00 00`, não `00 00 00 01`). Num host big-endian, um bug de
  ordem-nativa faria estes bytes divergirem — é o que a asserção proíbe.
- `written_store_reopens_with_identical_content` — o round-trip que a
  portabilidade protege: escrever → reabrir → conteúdo idêntico.

Verificação manual entre duas máquinas de arquiteturas diferentes (quando quiser
a prova ponta a ponta, além do teste unitário):

```bash
# Máquina A (ex. Windows x86-64):
embedmind --file portable.mind remember "cross-platform check" --global
sha256sum portable.mind          # anote o hash (certutil -hashfile no cmd puro)

# Copie portable.mind para a Máquina B (ex. Linux arm64) e lá:
sha256sum portable.mind          # DEVE bater com o hash da Máquina A
embedmind --file portable.mind recall "cross-platform" --all   # reencontra a memória
embedmind --file portable.mind stats                            # mesmas contagens
```

Hashes iguais confirmam G3 na prática; o `recall`/`stats` confirmam que o
arquivo idêntico também é semanticamente legível do outro lado. (Cada comando
que escreve — como o `remember` acima — faz checkpoint e **remove** o WAL
sidecar `portable.mind-wal` ao fechar o store, então após o `remember` resta só
o arquivo principal: copie apenas `portable.mind`. Um `portable.mind-wal`
residual só apareceria após um crash e seria reincorporado na próxima abertura.)

## Passos de publicação (dia do launch — `[MANUAL — founder]`)

Pré-requisitos: aumento de limite do crates.io concedido para `embedmind-core`;
`cargo login` feito; versão final definida (hoje `0.1.0-dev` em
`[workspace.package]` — trocar para a versão de release, ex. `0.1.0`, antes de
publicar).

```bash
cargo publish -p embedmind-core     # 1 — esperar aparecer no índice
cargo publish -p embedmind-mcp      # 2
cargo publish -p embedmind          # 3 (crate publica como "embedmind")
```

Entre passos, aguardar o crate anterior ficar disponível no índice (segundos a
poucos minutos). Cada versão publicada é **imutável** — não dá para republicar a
mesma versão; só é possível `yank`.
