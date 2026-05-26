mod session;
use session::Session;
enum Keys {
    String,
    nil
}

pub struct User {
    public_key: Keys,
    private_key: Keys,
    username: String,
    Session: Session, 
}
