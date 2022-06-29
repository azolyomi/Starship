import pymongo
import os
from dotenv import load_dotenv
load_dotenv()

connectionString = os.getenv('DB_CONNECT_STRING')
client = pymongo.MongoClient(connectionString)
db = client.StarshipDB
ServerConfigs = {}
