# Step 1: Select a base image
FROM golang:latest

# Step 2: Set environment variables (optional)

# Step 3: Update and install necessary packages
RUN apt-get update && \
    apt-get install -y \
        build-essential \
        curl \
        wget \
        git \
        openssh-server \
        pkg-config \
        libssl-dev \
        gcc \
        g++ \
        make\
				postgresql\
				postgresql-contrib
        # Add any additional packages you need

USER postgres

# Create a default database (optional)
RUN /etc/init.d/postgresql start && \
    psql --command "CREATE USER docker WITH SUPERUSER PASSWORD 'docker';" && \
    createdb -O docker audioshare

# Expose the PostgreSQL port
EXPOSE 5432

# Add an entry point script to start PostgreSQL when the container starts
USER root
COPY docker_entrypoint.sh /docker_entrypoint.sh
RUN chmod +x /docker_entrypoint.sh
#ENTRYPOINT ["/docker_entrypoint.sh"]

RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y


ENV PATH $PATH:/root/.cargo/bin

#RUN curl --proto '=https' --tlsv1.2 -sSfy https://sh.rustup.rs | sh
#RUN 

# Step 4: Set up your working directory
WORKDIR /app

# Step 5: Copy project files into the container (if necessary)
# COPY . /app
#COPY . /app
#COPY /Users/christianrichmond/.ssh/id_rsa /Users/christianrichmond/.ssh/id_rsa.pub /Users/christianrichmond/.ssh/config /root/.ssh/ 
#COPY /Users/christianrichmond/.vimrc  ./

# Step 6: Set a command to run (optional)
#CMD source $HOME/.cargo/env
