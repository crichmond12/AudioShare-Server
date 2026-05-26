#!/opt/homebrew/bin/python3
import sys
import sqlite3

# Path to the SQLite database file
DATABASE_FILE = "./../audioshare.db"

def apply_migrations(migration_files):
    # Connect to SQLite database
    conn = sqlite3.connect(DATABASE_FILE)
    cursor = conn.cursor()

    try:
        # Apply each migration file
        for filename in migration_files:
            with open(filename, "r") as file:
                migration_sql = file.read()
            cursor.executescript(migration_sql)
            print(f"Applied migration: {filename}")

        print("All migrations applied successfully.")
    finally:
        # Close database connection
        conn.close()

if __name__ == "__main__":
    # Get migration files from command-line arguments
    migration_files = sys.argv[1:]
    if not migration_files:
        print("Usage: python migrate.py <migration_file1> <migration_file2> ...")
        sys.exit(1)

    apply_migrations(migration_files)

