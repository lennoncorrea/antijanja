# AntiJanja

Compartilhamento de tela **100% P2P**, sem servidor central e sem conta.

**Quem transmite envia um código. Quem assiste cola o código. Pronto.**

## Como funciona

| Parte | Como |
|---|---|
| Vídeo e áudio | WebRTC (WebView2), criptografado com DTLS-SRTP, direto entre os computadores |
| Encontro dos peers | DHT Mainline do BitTorrent (BEP44), usada como "caixa postal" criptografada — em Rust |
| Descoberta de IP público | STUN público (só informa o IP; nenhum dado de vídeo passa por ele) |

O código `AJ2.…` carrega 32 bytes aleatórios. Dele o app deriva as chaves de assinatura e a chave
de criptografia (ChaCha20-Poly1305) das mensagens trocadas na DHT. Os nós da DHT só veem dados
criptografados; sem o código ninguém lê ou forja mensagens.

1. O host gera o código e fica consultando a DHT.
2. O espectador cola o código, cria uma oferta WebRTC e a publica na DHT.
3. O host encontra a oferta sozinho, responde e a conexão P2P direta é estabelecida.

O encontro leva em média ~15–20 s (latência da DHT). Depois disso a DHT não participa mais.

**Limitações:** se os dois lados estiverem atrás de NAT simétrico / CGNAT, a conexão direta pode
falhar — não há relay, por design. Qualquer pessoa com o código pode entrar; use *gerar novo código*
para invalidar o anterior (quem já está assistindo continua).

## Desenvolvimento

Requisitos: Rust (stable), Node.js, WebView2 (já vem no Windows 10/11).

```bash
npm install
npm run dev      # modo desenvolvimento (Vite + Tauri)
npm run build    # instalador em src-tauri/target/release/bundle/nsis

cd src-tauri
cargo test                           # testes unitários
cargo test -- --ignored --nocapture  # testes contra a DHT real (precisa de internet)
```
