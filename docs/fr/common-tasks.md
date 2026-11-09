# Tâches courantes

Des réponses courtes à « comment faire … ? ». Chaque réponse tient en une ligne
et renvoie à la page qui contient le code. Les pages partent de l'application de
[Bien démarrer](getting-started.md).

Certaines tâches n'ont pas encore d'exemple complet à copier et lancer. Ces
lignes renvoient à la page la plus proche et indiquent « (exemple complet à
venir) ».

## Table des matières

- [Tâches](#tâches)
- [Plus de recettes : le livre de recettes](#plus-de-recettes--le-livre-de-recettes)

## Tâches

| Tâche | Réponse | À lire |
|---|---|---|
| Inscrire un utilisateur | Vérifiez le mot de passe avec `passwords::strength_score`, hachez-le avec `passwords::hash` et stockez le hash sur la ligne de l'utilisateur. | [Bien démarrer, étape 14](getting-started.md#étape-14--ajouter-lauthentification-jwt) · [Hachage à l'inscription](auth-passwords.md#hachage-à-linscription) (exemple complet à venir) |
| Se connecter | Vérifiez le mot de passe avec `passwords::verify`, puis ouvrez une session ou émettez un JWT. | [Bien démarrer, étape 14](getting-started.md#étape-14--ajouter-lauthentification-jwt) · [Vérification à la connexion](auth-passwords.md#vérification-à-la-connexion) · [Sessions](auth-sessions.md) (exemple complet à venir) |
| Réinitialiser un mot de passe | `PasswordReset::issue` crée un lien signé à envoyer par e-mail ; `confirm_password_reset_pool` le vérifie et stocke le nouveau hash. | [Réinitialisation du mot de passe](auth-flows.md#réinitialisation-du-mot-de-passe) (exemple complet à venir) |
| Téléverser un fichier | `save_uploads` vérifie le fichier et l'écrit dans un `Storage` ; vous enregistrez la clé renvoyée sur un modèle. | [Téléverser un avatar de bout en bout](files.md#téléverser-un-avatar-de-bout-en-bout) |
| Envoyer un e-mail | Construisez un `Email`, choisissez un mailer et appelez `email.send(&mailer)`. | [Envoyer des e-mails](email.md#étape-1--construire-un-e-mail) |
| Lancer une tâche d'arrière-plan | Implémentez `Job`, enregistrez-la sur une file, démarrez la file et appelez `dispatch` depuis un handler. | [Tâches d'arrière-plan](jobs.md#étape-1--définir-une-tâche) |
| Tester une vue | Enveloppez votre routeur dans un `TestClient` et envoyez des requêtes dans le processus, sans serveur. | [Tests](testing.md#étape-1--pilotez-votre-application-avec-testclient) |
| Déployer | Définissez `RUSTANGO_ENV=prod`, lancez `migrate` et `check --deploy`, puis compilez avec `cargo build --release`. | [Bien démarrer, étape 18](getting-started.md#étape-18--déployer-en-production) · [Auditer avant de déployer](security.md#auditer-avant-de-déployer) |

## Plus de recettes : le livre de recettes

Le [livre de recettes](https://github.com/ujeenet/rustango/blob/main/crates/rustango/examples/cookbook_blog/COOKBOOK.md)
(en anglais) est une application de blog exécutable avec des recettes pour les
modèles, l'ORM, les migrations, l'admin, les API REST, le multi-tenancy et plus.
Les recettes sont couvertes par des tests. Pour le multi-tenancy lui-même,
commencez par [Multi-tenancy](tenancy.md).
