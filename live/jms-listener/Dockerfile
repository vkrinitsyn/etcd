FROM maven:3.9.9-eclipse-temurin-21 AS build
WORKDIR /b
COPY pom.xml .
RUN mvn -q -B dependency:go-offline
COPY src src
RUN mvn -q -B package -DskipTests

FROM eclipse-temurin:21-jre
COPY --from=build /b/target/jms-listener-1.jar /app/
COPY --from=build /b/target/lib /app/lib
ENTRYPOINT ["java", "-cp", "/app/jms-listener-1.jar:/app/lib/*", "e2j.Listen"]
