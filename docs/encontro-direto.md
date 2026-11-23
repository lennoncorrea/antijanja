# Encontro direto: pedido e resposta sem passar pela DHT

Para entrar numa transmissão, o espectador manda um pedido (a oferta do WebRTC) e o host
devolve a resposta. Pela DHT são quatro passos lentos: o espectador grava, o host lê, o host
grava, o espectador lê. Gravar na DHT leva segundos e às vezes vai para nós que o outro
lado não encontra: o sintoma era "O host não respondeu" depois de 90 s.

O encontro direto manda o pedido e a resposta direto entre os dois apps, pelo socket da
fofoca (docs/gossip-ao-vivo.md). Ele corre junto com o encontro pela DHT, que continua
igual; vale a resposta que chegar primeiro. Nenhum servidor novo.

## Como funciona

1. **Ticket.** Ao começar a transmissão, o host grava na DHT um ticket com os endereços
   onde pode ser chamado (o público, visto pelo STUN ou pelos vizinhos, e o da rede local).
   Ele confere os endereços a cada 5 s e grava de novo quando mudam (e a cada 10 min).
2. **Pedido direto.** Enquanto escolhe o slot, o espectador lê o ticket (ler da DHT é bem
   mais confiável que gravar) e passa a mandar o pedido direto para esses endereços, de
   segundo em segundo.
3. **Batida.** Atrás de um roteador, o host só recebe pacotes de quem ele já chamou. Por
   isso o espectador também "bate à porta" pela fofoca: a batida corre de vizinho em
   vizinho até o host, que chama o espectador de volta. Com os dois lados mandando, os
   roteadores deixam passar.
4. **Resposta.** O host responde direto para o endereço de onde o pedido veio (e também
   pela DHT, como antes). Pedido repetido não é entregue de novo, mas a resposta vai de
   novo, caso tenha se perdido.

Na mesma rede local, o endereço local do ticket já basta (sem batida).

### Roteador que troca a porta (NAT simétrico)

- O espectador guarda de onde o "olá" do host veio e passa a mandar o pedido para lá
  também: com NAT simétrico no host, essa porta não é a do ticket.
- Os "olás" (do host, depois da batida; do espectador, até aprender o endereço do host)
  vão também para as 10 portas acima e abaixo da porta pública conhecida: esses
  roteadores costumam dar portas em sequência a cada destino novo.
- No Início, o app já prepara a entrada nas transmissões à vista (até 3, amigos
  primeiro, sem senha): lê o ticket, escolhe o slot e bate à porta antes do clique.

## IPv6

Muitas operadoras já dão IPv6, e no IPv6 não há roteador traduzindo endereço (NAT):
quem fica preso atrás de CGNAT ou NAT simétrico no IPv4 costuma passar direto pelo IPv6.

- O ticket e a batida levam também o endereço IPv6 global do app (um socket IPv6 só para
  o encontro direto; a fofoca em si continua em IPv4).
- As conexões WebRTC escutam em IPv4 e IPv6 (`transport::UDP_BIND`): o ICE testa os
  dois e fica com o que conectar.

## Porta aberta no roteador (UPnP)

Quando o roteador da casa aceita UPnP, o próprio app abre portas nele (`src/upnp.rs`),
sempre com prazo de 1 h (renovado enquanto o app está aberto; roteador que só aceita
porta aberta para sempre fica sem):

- a porta da fofoca: o endereço aberto vai primeiro no ticket e no anúncio do tópico, e
  pedidos diretos e vizinhos chegam sem precisar de batida;
- as portas de cada conexão WebRTC: o endereço aberto entra na oferta/resposta como mais
  um candidato, e o outro lado conecta por ele mesmo com NAT simétrico.

Roteador sem IP público (atrás de outro roteador, ou CGNAT da operadora): abrir porta não
adianta e o app nem tenta. A busca pelo roteador é feita em segundo plano ao abrir o app;
a conexão nunca espera por ela. Desligado nos testes e com `AJ_NO_UPNP=1`.

## Mensagens (na fofoca)

- `knock`: `k` = marca da transmissão, `e` = endereços de quem bate (cifrados), `h` =
  saltos. Só vizinhos confirmados repassam; cada batida passa uma vez por app.
- `rv`: pedaço de um pedido (`r = 'v'`) ou de uma resposta (`r = 'h'`), com o número da
  mensagem (`m`), o pedaço (`i` de `n`) e o slot do pedido na DHT (`s`). `n = 0` só abre a
  passagem pelo roteador.

## Segurança

- Tudo é cifrado com a chave do código (a mesma do encontro pela DHT). Quem não tem o
  código não lê nem forja pedidos ou respostas, e a batida não mostra o endereço de quem
  bate para os apps no caminho.
- A marca da transmissão vem da chave dos espectadores, que muda com a senha: com a senha
  errada, o host nem reconhece o pedido (como na DHT).
- O ticket é assinado pela chave do host (só ele grava) e cifrado (só quem tem o código
  lê os endereços).

## Limites

- Os dois atrás de NAT simétrico (comum em redes de celular): a passagem não abre e fica
  valendo o caminho pela DHT.
- A batida precisa que os dois apps estejam na fofoca (com vizinhos). Sem ela, o pedido
  direto só chega se o host aceitar pacotes de fora (porta aberta no roteador, ou na mesma
  rede local).
