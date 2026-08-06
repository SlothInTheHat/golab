SESSION_TTL = 900


class SessionStore:
    def __init__(self):
        self._sessions = {}

    def create(self, user_id):
        token = f"tok_{user_id}"
        self._sessions[token] = user_id
        return token

    def resolve(self, token):
        return self._sessions.get(token)


def authenticate(username, password):
    if not username or not password:
        return None
    return SessionStore().create(username)


def require_auth(token, store):
    return store.resolve(token) is not None
