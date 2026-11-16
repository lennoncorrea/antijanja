# AntiJanja

Compartilhamento de tela **100% P2P**, sem servidor central e sem conta.

**Quem transmite envia um código. Quem assiste cola o código. Pronto.**

## Como funciona

| Parte | Como |
|---|---|
| Vídeo e áudio | WebRTC nativo (webrtc-rs), criptografado com DTLS-SRTP, direto entre os computadores |
| Encontro dos peers | DHT Mainline do BitTorrent (BEP44), usada como "caixa postal" criptografada — em Rust |
| Descoberta de IP público | STUN público (só informa o IP; nenhum dado de vídeo passa por ele) |

O código `AJ3.…` é a chave pública (Ed25519) de um par sorteado pelo host a cada código. Dele o
app deriva a chave dos espectadores e a chave de criptografia (ChaCha20-Poly1305) das mensagens
trocadas na DHT. Os nós da DHT só veem dados criptografados; sem o código ninguém lê nem envia
mensagens. A chave privada fica só no computador do host: quem tem o código pode pedir para
assistir, mas não consegue responder no lugar do host.

**Lista pública:** cada entrada é assinada pela chave do host da transmissão anunciada (ninguém
anuncia, encerra ou renomeia a transmissão de outra pessoa) e as vagas mudam de endereço a cada
10 min (quem travar uma vaga só a trava até a próxima troca). Sem servidor, não dá para impedir
que alguém sobrescreva vagas sem parar.

1. O host gera o código e fica consultando a DHT.
2. O espectador cola o código, cria uma oferta WebRTC e a publica na DHT.
3. O host encontra a oferta sozinho, responde e a conexão P2P direta é estabelecida.

O encontro leva em média ~15–20 s (latência da DHT). Depois disso a DHT não participa mais.

**Limitações:** se os dois lados estiverem atrás de NAT simétrico / CGNAT, a conexão direta pode
falhar — não há relay, por design. Qualquer pessoa com o código pode entrar; use *gerar novo código*
para invalidar o anterior (quem já está assistindo continua).

## Desenvolvimento

Tudo em Rust, sem navegador embutido: a interface é feita em [Slint](https://slint.dev)
(arquivos em `ui/`) e o motor (captura, H.264, WebRTC, DHT, áudio, player) fica em `src/`.

Requisitos: Rust (stable). Para gerar o instalador: NSIS e Node.js (só para assinar a atualização).

```powershell
cargo run                        # roda o app (modo desenvolvimento)
cargo test                       # testes unitários
cargo test -- --ignored --nocapture  # testes contra a DHT real (precisa de internet)

.\scripts\release.ps1            # build + instalador assinado + latest.json (sem publicar)
```

Os builds rodam com poucas tarefas e prioridade baixa (`CARGO_BUILD_JOBS=4`) para não travar o PC.

### Releases

Cada PR da `dev` mergeado na `main` vira uma release automaticamente
(`.github/workflows/release.yml`): a action compila, assina, publica no GitHub e os apps
instalados se atualizam ao abrir. Ela precisa do secret `TAURI_SIGNING_PRIVATE_KEY`
(conteúdo de `~/.tauri/antijanja.key`).

As versões são `0.x.y` enquanto o app não for considerado estável. A action usa a versão do
`Cargo.toml` se ela ainda não foi lançada; senão soma 1 ao último número da última release
(`0.3.2` → `0.3.3`). Para pular para `0.4.0`, mude o `Cargo.toml` no PR.

Os apps antigos (2.x e 3.x) só atualizam para uma versão "maior" que a deles, então o
`latest.json` leva `version: 3.999.0` (lido só por eles) e a versão real em `app_version`.

Fontes: [Inter](https://rsms.me/inter/) e [JetBrains Mono](https://www.jetbrains.com/lp/mono/), ambas OFL (`assets/fonts`).
