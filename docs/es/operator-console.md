# La consola de operador

Un proyecto multi-tenant tiene dos clases de administrador, y nunca se mezclan:

- Los **operadores** llevan el *despliegue*. Viven en el registry, entran por el dominio apex y alcanzan a cada tenant.
- Los **usuarios de tenant** llevan *un* tenant. Viven en la base de datos de ese tenant y nunca ven la consola.

La consola de operador es la interfaz web de la primera clase — aprovisionar tenants, vincular nombres de host, gestionar operadores, leer el rastro de auditoría y sacar un tenant de servicio. Todo eso es también un [verbo de `manage`](manage.md#comandos-de-tenancy), porque una acción que solo existe en una superficie no se puede automatizar, y una que solo existe en una shell no se puede delegar.

[![La lista de tenants de la consola de operador — cada tenant del registry con su modo de almacenamiento, patrón de host y estado activo, más las acciones de aprovisionar, migrar y precalentar](../img/operator-console.png)](../img/operator-console.png)

## Tabla de contenidos

- [Montarla](#montarla)
- [Qué hace cada página](#qué-hace-cada-página)
- [Nombres de host](#nombres-de-host)
- [Operadores](#operadores)
- [El registro de auditoría](#el-registro-de-auditoría)
- [Ejecuciones de aprovisionamiento](#ejecuciones-de-aprovisionamiento)
- [Los tres niveles de capacidad](#los-tres-niveles-de-capacidad)

---

## Montarla

La consola es un router que montas; cuánto puede hacer depende de lo que le entregues.

```rust
use rustango::tenancy::operator_console::{router, router_with_pools, router_with_provisioning, SessionSecret};

// Solo lectura: recorrer tenants, operadores y el registro de auditoría.
let app = router(registry.clone(), SessionSecret::from_env_or_random());

// …más editar tenants, gestionar operadores, vincular hosts, precalentar pools.
let app = router_with_pools(registry.clone(), pools.clone(), secret);

// …más aprovisionar tenants nuevos y ejecutar migraciones.
let app = router_with_provisioning(registry.clone(), pools.clone(), provisioner, secret);
```

En un proyecto `tenant` generado ya viene cableado — `Cli::new().tenancy().with_tenant_provisioning("migrations")` monta la versión completa. Véase [Andamiaje](scaffolding.md).

La consola responde en el dominio **apex** (`RUSTANGO_APEX_DOMAIN`), no en un subdominio de tenant: `http://localhost:8080/login` con apex `localhost`. Una petición a `127.0.0.1` no es el apex y no encajará.

---

## Qué hace cada página

| Página | Para qué sirve |
|---|---|
| **Organizations** | Cada tenant, con modo de almacenamiento, patrón de host y estado activo |
| **Organizations → Edit** | Nombre visible, patrón de host, prefijo de ruta, puerto, URL de base, marca |
| **Hostnames** | Los dominios adicionales a los que responde un tenant |
| **Operators** | Quién puede entrar en esta consola |
| **Audit log** | Cada cambio hecho a través de la consola |
| **Runs** | Ejecuciones de aprovisionamiento y migración, en directo |

---

## Nombres de host

Un tenant es alcanzable en su subdominio y, opcionalmente, en los nombres de host adicionales que le vincules. Un nombre de host lleva a exactamente un tenant.

[![La página de nombres de host de un tenant — el host base marcado como tal y no borrable, hosts adicionales con acciones Aparcar y Quitar, y un formulario para añadir uno](../img/operator-console-hosts.png)](../img/operator-console-hosts.png)

Dos cosas que conviene saber:

**El host base no tiene botón de borrado.** Viene de la columna `host_pattern` del tenant, no de la tabla de hosts, así que no hay fila que quitar — cámbialo en la página de edición del tenant. Eso es estructural, no una regla de interfaz: el motor también lo rechaza, de modo que quien adivine el `POST` obtiene la misma respuesta que quien lee la página.

**Aparcar conserva la fila.** *Park* saca un host de servicio sin perder el registro — útil mientras el DNS propaga, o al retirar un dominio que quizá quieras recuperar. *Serve* lo devuelve al servicio.

Los nombres de host se normalizan a la entrada: minúsculas, sin esquema, sin puerto, sin ruta. El valor guardado se compara byte a byte con la cabecera `Host`, así que un valor que nunca podría encajar se rechaza en vez de guardarse.

Desde la CLI: [`list-hosts` / `add-host` / `remove-host` / `set-host-enabled`](manage.md#nombres-de-host).

---

## Operadores

[![La página de operadores — el operador conectado marcado como «eres tú», con un formulario para añadir otro y la opción de generar una contraseña](../img/operator-console-operators.png)](../img/operator-console-operators.png)

Los operadores se desactivan, nunca se borran: la fila permanece, así que un «¿quién hizo esto?» posterior sigue resolviéndose. La consola la relee en **cada** petición, de modo que una desactivación surte efecto en el siguiente clic y no cuando caduque la cookie.

Dos cosas que la página no permite, por la misma razón:

- **Desactivarte a ti mismo.** Tu siguiente petición sería rechazada.
- **Desactivar al último operador activo.** Eso deja a todo el mundo fuera, y solo una shell sobre el registry podría deshacerlo.

Una contraseña generada se muestra **una vez**, en el cuerpo de la respuesta — nunca mediante una redirección, que la pondría en la barra de direcciones, el historial, el referente y cada log de acceso por el camino.

Desde la CLI: [`list-operators` / `set-operator-active`](manage.md#list-operators).

---

## El registro de auditoría

[![El registro de auditoría de la consola — quién cambió qué, en qué registro y cuándo, filtrable por entidad, id y operación](../img/operator-console-audit.png)](../img/operator-console-audit.png)

Cada mutación hecha por la consola queda anotada: ediciones de tenant, cambios de hosts, gestión de operadores, suplantación, purgas, precalentados. La columna `source` lleva `operator:<id>:<verbo>`, de forma que la actividad de los operadores se puede separar después de la de los usuarios de tenant.

Este es el registro del **registry**. El historial propio de un tenant vive en el admin de ese tenant — mezclarlos obligaría a abrir en abanico una consulta sobre cada pool de tenant para pintar una sola página.

Crece con el uso de la consola, así que recórtalo periódicamente con [`audit-cleanup`](manage.md#audit-cleanup), que barre el registro del registry y el de cada tenant activo.

Desde la CLI: [`audit-log`](manage.md#ver-qué-ha-pasado).

---

## Ejecuciones de aprovisionamiento

[![La página de ejecuciones de aprovisionamiento — cada ejecución con su tipo, tenant, estado y tiempos, enlazando a los pasos registrados](../img/operator-console-runs.png)](../img/operator-console-runs.png)

Aprovisionar un tenant son varios pasos contra una base de datos que puede ser lenta o inalcanzable, así que se registra como una **ejecución** y no como una petición que vuelve o no vuelve. Cada ejecución emite sus pasos en directo y sobrevive a una recarga, a una reconexión o a que la mire un segundo pod.

Los tenants creados desde la CLI también quedan registrados, marcados con `requested_by = cli`, así que el historial cubre ambas superficies.

Desde la CLI: [`list-runs` / `show-run`](manage.md#ver-qué-ha-pasado).

---

## Los tres niveles de capacidad

Qué rutas existen depende del constructor que hayas montado:

| | `router` | `router_with_pools` | `router_with_provisioning` |
|---|---|---|---|
| Recorrer tenants, operadores, auditoría | ✓ | ✓ | ✓ |
| Editar un tenant, gestionar hosts | | ✓ | ✓ |
| Añadir / desactivar operadores | | ✓ | ✓ |
| Precalentar pools | | ✓ | ✓ |
| Sacar un tenant de servicio | | ✓ | ✓ |
| Aprovisionar un tenant, ejecutar migraciones | | | ✓ |
| Ver las ejecuciones de aprovisionamiento | | | ✓ |

Las rutas no montadas devuelven 404 en lugar de 403 — una consola de solo lectura no anuncia lo que no puede hacer.

**Cada operador lo puede todo.** No hay controles de permisos por operador: un operador alcanza cada tenant y cada acción que la consola ofrece. La frontera de control de acceso es la propia lista de operadores — justo por eso una desactivación surte efecto en la siguiente petición.
